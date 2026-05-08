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
//!   - `status.selected_repo`                   — Git tab selected repository
//!                                                root, workdir-relative (`.` for root).
//!   - `commit.files_tree_mode`                 — Graph tab commit files list/tree
//!   - `ui.theme`                               — `dark` | `light` | `auto` (default `auto`).
//!                                                Read once in `main.rs` before raw-mode
//!                                                entry; `auto` probes the terminal's
//!                                                background via OSC 11.
//!   - `quickopen.mru`                          — Quick-open palette MRU; tab-separated
//!                                                workdir-relative paths, newest-first,
//!                                                capped at 50 entries. Written on every
//!                                                accept, read once in `App::new`.
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

pub fn remove(key: &str) {
    let mut map = read_all();
    if map.remove(key).is_some() {
        write_all(&map);
    }
}

/// Pair of [`get_bool`]: writes `"true"` / `"false"` so the value
/// round-trips. Callers should prefer this over building the literal
/// string at every flip site.
pub fn set_bool(key: &str, value: bool) {
    set(key, if value { "true" } else { "false" });
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;
    use tempfile::TempDir;
    use test_support::{HOME_LOCK, HomeGuard};

    /// Workspace-shared HOME_LOCK so this serialises against any other
    /// test in the same `cargo test --lib` binary that touches HOME.
    fn isolated_home() -> (MutexGuard<'static, ()>, HomeGuard, TempDir) {
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let home = HomeGuard::enter(tmp.path());
        (lock, home, tmp)
    }

    #[test]
    fn get_missing_key_returns_none() {
        let (_lock, _home, _tmp) = isolated_home();
        assert_eq!(get("nothing"), None);
        assert!(!get_bool("nothing"));
    }

    #[test]
    fn set_then_get_roundtrip() {
        let (_lock, _home, _tmp) = isolated_home();
        set("diff.layout", "side_by_side");
        assert_eq!(get("diff.layout").as_deref(), Some("side_by_side"));
    }

    #[test]
    fn set_preserves_unrelated_keys() {
        // Regression: an earlier `save_prefs` rewrote the file with only
        // `layout=` / `mode=`, wiping `status.tree_mode` and friends every
        // time the user pressed `m` on the Git tab.
        let (_lock, _home, _tmp) = isolated_home();
        set("status.tree_mode", "true");
        set("commit.diff_layout", "side_by_side");
        set("diff.layout", "unified"); // the "pressing m" moment
        assert_eq!(get("status.tree_mode").as_deref(), Some("true"));
        assert_eq!(get("commit.diff_layout").as_deref(), Some("side_by_side"));
        assert_eq!(get("diff.layout").as_deref(), Some("unified"));
    }

    #[test]
    fn get_bool_reads_true_false_and_default() {
        let (_lock, _home, _tmp) = isolated_home();
        set("a", "true");
        set("b", "false");
        assert!(get_bool("a"));
        assert!(!get_bool("b"));
        assert!(!get_bool("missing"));
    }

    #[test]
    fn migrate_is_noop_on_empty_home() {
        // First launch for a brand-new user: no prefs file, no git.prefs.
        // Migrator must NOT create an empty prefs file (otherwise the
        // tempdir snapshot tests show a spurious untracked file).
        let (_lock, _home, tmp) = isolated_home();
        migrate_legacy_prefs();
        let path = tmp.path().join(".config").join("reef").join("prefs");
        assert!(!path.exists(), "migrator created a spurious prefs file");
    }

    #[test]
    fn migrate_renames_unprefixed_layout_mode() {
        let (_lock, _home, tmp) = isolated_home();
        let dir = tmp.path().join(".config").join("reef");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("prefs"), "layout=side_by_side\nmode=full_file\n").unwrap();

        migrate_legacy_prefs();

        assert_eq!(get("diff.layout").as_deref(), Some("side_by_side"));
        assert_eq!(get("diff.mode").as_deref(), Some("full_file"));
        assert_eq!(get("layout"), None, "legacy key must be removed");
        assert_eq!(get("mode"), None, "legacy key must be removed");
    }

    #[test]
    fn migrate_folds_git_prefs_and_deletes_it() {
        // End-to-end: a pre-deplugin install has both files. One boot of
        // App::new() (which calls migrate_legacy_prefs at the top) must
        // leave every inline panel reading the user's real saved state.
        let (_lock, _home, tmp) = isolated_home();
        let dir = tmp.path().join(".config").join("reef");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("prefs"), "layout=side_by_side\nmode=full_file\n").unwrap();
        std::fs::write(
            dir.join("git.prefs"),
            "tree_mode=true\n\
             commit_diff_layout=side_by_side\n\
             commit_diff_mode=full_file\n\
             commit_files_tree_mode=true\n",
        )
        .unwrap();

        migrate_legacy_prefs();

        assert_eq!(get("diff.layout").as_deref(), Some("side_by_side"));
        assert_eq!(get("diff.mode").as_deref(), Some("full_file"));
        assert!(get_bool("status.tree_mode"));
        assert_eq!(get("commit.diff_layout").as_deref(), Some("side_by_side"));
        assert_eq!(get("commit.diff_mode").as_deref(), Some("full_file"));
        assert!(get_bool("commit.files_tree_mode"));
        assert!(
            !dir.join("git.prefs").exists(),
            "legacy git.prefs must be deleted after migration"
        );
    }

    #[test]
    fn migrate_is_idempotent() {
        // Second boot and every boot after: nothing to migrate, no fs write,
        // no change in prefs contents.
        let (_lock, _home, tmp) = isolated_home();
        set("diff.layout", "side_by_side");
        let path = tmp.path().join(".config").join("reef").join("prefs");
        let first = std::fs::read(&path).unwrap();

        migrate_legacy_prefs();
        migrate_legacy_prefs(); // run twice to be sure

        let second = std::fs::read(&path).unwrap();
        assert_eq!(first, second, "idempotent migration touched the file");
    }

    #[test]
    fn migrate_does_not_overwrite_existing_prefixed_keys() {
        // If the user somehow has both `layout=x` (legacy) AND
        // `diff.layout=y` (already-migrated) in the same file, the
        // already-migrated value wins — we never downgrade.
        let (_lock, _home, tmp) = isolated_home();
        let dir = tmp.path().join(".config").join("reef");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("prefs"),
            "diff.layout=unified\nlayout=side_by_side\n",
        )
        .unwrap();

        migrate_legacy_prefs();

        assert_eq!(
            get("diff.layout").as_deref(),
            Some("unified"),
            "prefixed value must be preserved"
        );
    }
}
