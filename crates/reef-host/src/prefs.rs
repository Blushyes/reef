//! Persistent preferences stored in `~/.config/reef/prefs` as a flat
//! `key=value` file, plus `migrate_legacy_prefs` — a one-shot, idempotent
//! migrator called from `App::new` that folds pre-1.0 unprefixed keys
//! (`layout=`, `mode=`) and the retired `~/.config/reef/git.prefs` file
//! into the current prefixed namespace.
//!
//! Key convention:
//!   - `diff.layout` / `diff.mode`             — Git tab right-side diff
//!   - `commit.diff_layout` / `commit.diff_mode` — Graph tab commit-file diff
//!   - `status.tree_mode`                       — Git tab left-side list/tree
//!   - `commit.files_tree_mode`                 — Graph tab commit files list/tree
//!
//! Writers must go through `set()` (not raw `std::fs::write`) so that
//! updating one key doesn't erase all the others.

use std::collections::BTreeMap;
use std::path::PathBuf;

fn prefs_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let dir = PathBuf::from(home).join(".config").join("reef");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("prefs"))
}

fn legacy_git_prefs_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("reef")
            .join("git.prefs"),
    )
}

pub fn read_all() -> BTreeMap<String, String> {
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

pub fn get(key: &str) -> Option<String> {
    read_all().get(key).cloned()
}

/// Read a bool pref: `true` iff the stored value is the literal string
/// `"true"`. Any other value (including missing) yields `false`.
pub fn get_bool(key: &str) -> bool {
    get(key).map(|v| v == "true").unwrap_or(false)
}

pub fn set(key: &str, value: &str) {
    let mut map = read_all();
    map.insert(key.to_string(), value.to_string());
    write_all(&map);
}

/// Fold unprefixed legacy keys into the new prefixed namespace and delete the
/// old `git.prefs` file. Safe to call repeatedly — already-migrated keys are
/// left alone and never overwrite explicit values. Writes only when state
/// actually changed, so a first boot in a clean HOME doesn't touch the fs.
pub fn migrate_legacy_prefs() {
    let original = read_all();
    let mut map = original.clone();
    let mut changed = false;

    if let Some(v) = map.remove("layout") {
        map.entry("diff.layout".into()).or_insert(v);
        changed = true;
    }
    if let Some(v) = map.remove("mode") {
        map.entry("diff.mode".into()).or_insert(v);
        changed = true;
    }

    if let Some(legacy) = legacy_git_prefs_path() {
        if let Ok(content) = std::fs::read_to_string(&legacy) {
            for line in content.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    let k = k.trim();
                    let v = v.trim();
                    let new_key = match k {
                        "tree_mode" => "status.tree_mode",
                        "commit_diff_layout" => "commit.diff_layout",
                        "commit_diff_mode" => "commit.diff_mode",
                        "commit_files_tree_mode" => "commit.files_tree_mode",
                        _ => continue,
                    };
                    map.entry(new_key.into()).or_insert_with(|| v.to_string());
                }
            }
            let _ = std::fs::remove_file(&legacy);
            changed = true;
        }
    }

    if changed {
        write_all(&map);
    }
}
