//! Persistent preferences stored in `~/.config/reef/prefs` as a flat
//! `key=value` file, plus a one-shot migrator that folds the legacy unprefixed
//! keys (from the pre-inline host and the retired `git.prefs` file) into the
//! new prefixed namespace.
//!
//! Key convention after migration:
//!   - `diff.layout` / `diff.mode`             — Git tab right-side diff
//!   - `commit.diff_layout` / `commit.diff_mode` — Graph tab commit-file diff
//!   - `status.tree_mode`                       — Git tab left-side list/tree
//!   - `commit.files_tree_mode`                 — Graph tab commit files list/tree
//!
//! The migrator is intentionally not called by `App::new()` yet; it will be
//! wired up once the inline panels are the source of truth (M4/M5), otherwise
//! the still-running git plugin would rewrite `git.prefs` right after we
//! deleted it.

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
    Some(PathBuf::from(home).join(".config").join("reef").join("git.prefs"))
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
pub fn get(key: &str) -> Option<String> {
    read_all().get(key).cloned()
}

#[allow(dead_code)]
pub fn set(key: &str, value: &str) {
    let mut map = read_all();
    map.insert(key.to_string(), value.to_string());
    write_all(&map);
}

/// Fold unprefixed legacy keys into the new prefixed namespace and delete the
/// old `git.prefs` file. Safe to call repeatedly — already-migrated keys are
/// left alone and never overwrite explicit values.
#[allow(dead_code)]
pub fn migrate_legacy_prefs() {
    let mut map = read_all();

    // Step 1: fold unprefixed host keys (`layout=`, `mode=`) into `diff.*`.
    if let Some(v) = map.remove("layout") {
        map.entry("diff.layout".into()).or_insert(v);
    }
    if let Some(v) = map.remove("mode") {
        map.entry("diff.mode".into()).or_insert(v);
    }

    // Step 2: fold `~/.config/reef/git.prefs` in, then delete it.
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
        }
    }

    write_all(&map);
}
