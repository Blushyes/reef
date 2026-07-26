//! Shared Reef host preferences stored in `~/.config/reef/prefs`.
//!
//! The TUI and native hosts use this module so a preference selected in one
//! renderer is restored by the other. Writers update one key without dropping
//! the rest of the flat `key=value` file.

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
    let Some(path) = prefs_path() else {
        return BTreeMap::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    reef_core::prefs::parse_flat(&content)
}

fn write_all(map: &BTreeMap<String, String>) {
    let Some(path) = prefs_path() else {
        return;
    };
    let _ = std::fs::write(path, reef_core::prefs::serialize_flat(map));
}

pub fn get(key: &str) -> Option<String> {
    read_all().get(key).cloned()
}

/// Returns `true` only when the persisted value is the literal `"true"`.
pub fn get_bool(key: &str) -> bool {
    reef_core::prefs::bool_value(&read_all(), key)
}

pub fn set(key: &str, value: &str) {
    let mut map = read_all();
    map.insert(key.to_string(), value.to_string());
    write_all(&map);
}

pub fn set_bool(key: &str, value: bool) {
    set(key, reef_core::prefs::bool_str(value));
}

/// Folds retired preference names into the current shared preference file.
/// The migration is idempotent and leaves a clean home directory untouched.
pub fn migrate_legacy_prefs() {
    let original = read_all();
    let legacy = legacy_git_prefs_path();
    let legacy_content = legacy
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok());
    let migration = reef_core::prefs::migrate_legacy(original.clone(), legacy_content.as_deref());
    if migration.delete_legacy_git_prefs
        && let Some(path) = legacy
    {
        let _ = std::fs::remove_file(path);
    }
    if migration.changed && migration.map != original {
        write_all(&migration.map);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;
    use tempfile::TempDir;
    use test_support::{HOME_LOCK, HomeGuard};

    fn isolated_home() -> (MutexGuard<'static, ()>, HomeGuard, TempDir) {
        let lock = HOME_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let temp = TempDir::new().unwrap();
        let home = HomeGuard::enter(temp.path());
        (lock, home, temp)
    }

    #[test]
    fn set_preserves_unrelated_keys() {
        let (_lock, _home, _temp) = isolated_home();
        set("status.tree_mode", "true");
        set("commit.diff_layout", "side_by_side");
        set("diff.layout", "unified");

        assert_eq!(get("status.tree_mode").as_deref(), Some("true"));
        assert_eq!(get("commit.diff_layout").as_deref(), Some("side_by_side"));
        assert_eq!(get("diff.layout").as_deref(), Some("unified"));
    }

    #[test]
    fn bool_values_roundtrip() {
        let (_lock, _home, _temp) = isolated_home();
        set_bool("status.tree_mode", true);
        assert!(get_bool("status.tree_mode"));

        set_bool("status.tree_mode", false);
        assert!(!get_bool("status.tree_mode"));
    }

    #[test]
    fn migration_folds_legacy_git_preferences() {
        let (_lock, _home, temp) = isolated_home();
        let dir = temp.path().join(".config").join("reef");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("git.prefs"), "tree_mode=true\n").unwrap();

        migrate_legacy_prefs();

        assert!(get_bool(reef_core::prefs::STATUS_TREE_MODE));
        assert!(!dir.join("git.prefs").exists());
    }
}
