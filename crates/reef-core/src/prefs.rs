use std::collections::BTreeMap;

pub const DIFF_LAYOUT: &str = "diff.layout";
pub const DIFF_MODE: &str = "diff.mode";
pub const STATUS_TREE_MODE: &str = "status.tree_mode";
pub const COMMIT_DIFF_LAYOUT: &str = "commit.diff_layout";
pub const COMMIT_DIFF_MODE: &str = "commit.diff_mode";
pub const COMMIT_FILES_TREE_MODE: &str = "commit.files_tree_mode";
pub const UI_THEME: &str = "ui.theme";
pub const EDITOR_COMMAND: &str = "editor.command";
pub const HOSTS_RECENT: &str = "hosts.recent";
pub const GRAPH_SCOPE: &str = "graph.scope";
pub const GRAPH_SCOPE_RECENT: &str = "graph.scope.recent";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemePref {
    #[default]
    Auto,
    Dark,
    Light,
}

impl ThemePref {
    pub fn pref_str(self) -> &'static str {
        match self {
            ThemePref::Auto => "auto",
            ThemePref::Dark => "dark",
            ThemePref::Light => "light",
        }
    }

    pub fn from_pref_str(s: &str) -> Self {
        match s {
            "dark" => ThemePref::Dark,
            "light" => ThemePref::Light,
            _ => ThemePref::Auto,
        }
    }

    pub fn next(self) -> Self {
        match self {
            ThemePref::Auto => ThemePref::Dark,
            ThemePref::Dark => ThemePref::Light,
            ThemePref::Light => ThemePref::Auto,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub map: BTreeMap<String, String>,
    pub changed: bool,
    pub delete_legacy_git_prefs: bool,
}

pub fn parse_flat(body: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in body.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

pub fn serialize_flat(map: &BTreeMap<String, String>) -> String {
    let mut content = String::new();
    for (k, v) in map {
        content.push_str(k);
        content.push('=');
        content.push_str(v);
        content.push('\n');
    }
    content
}

pub fn bool_value(map: &BTreeMap<String, String>, key: &str) -> bool {
    map.get(key).map(|v| v == "true").unwrap_or(false)
}

pub fn bool_str(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

pub fn migrate_legacy(
    current: BTreeMap<String, String>,
    legacy_git_prefs: Option<&str>,
) -> Migration {
    let mut map = current;
    let mut changed = false;
    let mut delete_legacy_git_prefs = false;

    if let Some(v) = map.remove("layout") {
        map.entry(DIFF_LAYOUT.into()).or_insert(v);
        changed = true;
    }
    if let Some(v) = map.remove("mode") {
        map.entry(DIFF_MODE.into()).or_insert(v);
        changed = true;
    }

    if let Some(content) = legacy_git_prefs {
        for line in content.lines() {
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                let v = v.trim();
                let new_key = match k {
                    "tree_mode" => STATUS_TREE_MODE,
                    "commit_diff_layout" => COMMIT_DIFF_LAYOUT,
                    "commit_diff_mode" => COMMIT_DIFF_MODE,
                    "commit_files_tree_mode" => COMMIT_FILES_TREE_MODE,
                    _ => continue,
                };
                map.entry(new_key.into()).or_insert_with(|| v.to_string());
            }
        }
        delete_legacy_git_prefs = true;
        changed = true;
    }

    Migration {
        map,
        changed,
        delete_legacy_git_prefs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_serialize_flat_roundtrip() {
        let map = parse_flat(" b = two \na=one\nbad\n");
        assert_eq!(map.get("a").map(String::as_str), Some("one"));
        assert_eq!(map.get("b").map(String::as_str), Some("two"));
        assert_eq!(serialize_flat(&map), "a=one\nb=two\n");
    }

    #[test]
    fn bool_helpers_use_true_literal_only() {
        let map = parse_flat("a=true\nb=false\nc=yes\n");
        assert!(bool_value(&map, "a"));
        assert!(!bool_value(&map, "b"));
        assert!(!bool_value(&map, "c"));
        assert_eq!(bool_str(true), "true");
        assert_eq!(bool_str(false), "false");
    }

    #[test]
    fn migration_renames_unprefixed_keys() {
        let migrated = migrate_legacy(parse_flat("layout=side_by_side\nmode=full_file\n"), None);
        assert!(migrated.changed);
        assert_eq!(
            migrated.map.get(DIFF_LAYOUT).map(String::as_str),
            Some("side_by_side")
        );
        assert_eq!(
            migrated.map.get(DIFF_MODE).map(String::as_str),
            Some("full_file")
        );
        assert!(!migrated.map.contains_key("layout"));
        assert!(!migrated.map.contains_key("mode"));
    }

    #[test]
    fn migration_folds_legacy_git_prefs() {
        let legacy = "tree_mode=true\ncommit_diff_layout=side_by_side\ncommit_diff_mode=full_file\ncommit_files_tree_mode=true\n";
        let migrated = migrate_legacy(BTreeMap::new(), Some(legacy));
        assert!(migrated.changed);
        assert!(migrated.delete_legacy_git_prefs);
        assert_eq!(
            migrated.map.get(STATUS_TREE_MODE).map(String::as_str),
            Some("true")
        );
        assert_eq!(
            migrated.map.get(COMMIT_DIFF_LAYOUT).map(String::as_str),
            Some("side_by_side")
        );
    }

    #[test]
    fn migration_does_not_overwrite_prefixed_keys() {
        let migrated = migrate_legacy(
            parse_flat("diff.layout=unified\nlayout=side_by_side\n"),
            None,
        );
        assert_eq!(
            migrated.map.get(DIFF_LAYOUT).map(String::as_str),
            Some("unified")
        );
    }

    #[test]
    fn theme_pref_cycles() {
        assert_eq!(ThemePref::from_pref_str("dark"), ThemePref::Dark);
        assert_eq!(ThemePref::from_pref_str("wat"), ThemePref::Auto);
        assert_eq!(ThemePref::Auto.next(), ThemePref::Dark);
        assert_eq!(ThemePref::Dark.next(), ThemePref::Light);
        assert_eq!(ThemePref::Light.next(), ThemePref::Auto);
    }
}
