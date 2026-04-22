//! `Ctrl+O` hosts picker state.
//!
//! Modeled after `quick_open`: an overlay prompt that owns the keyboard
//! while `active`, with its own filter + selection. The picker produces
//! an `SshTarget` (host string ± `:path`) which the main loop uses to
//! build a fresh `RemoteBackend` and swap the running `App` for a new
//! one via the outer `'session:` loop.

use ratatui::layout::Rect;

use crate::hosts::HostEntry;

/// Maximum number of recent hosts we persist in prefs. Anything beyond
/// that becomes noise; users with more than five ssh contexts can always
/// type the full target.
pub const MAX_RECENT: usize = 5;

/// Preference key where we store the recent-host list. Tab-separated so
/// the flat-kv prefs file stays parseable even when a target contains a
/// `:` (it can't contain `\t`).
pub const RECENT_PREF_KEY: &str = "hosts.recent";

/// What the picker is asking for: either filtering by alias or typing a
/// literal `user@host[:path]` target directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Typing filters the list of parsed config entries.
    Search,
    /// Typing builds a raw `[user@]host[:path]` target; Enter commits.
    Path,
}

/// Resolved target the picker hands back to `main.rs`. Same shape as
/// `--ssh` argv so the existing `build_ssh_backend` helper can take it
/// without reparsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    /// Remote workdir path. `.` if the user didn't specify one (matches
    /// `split_ssh_target`'s default).
    pub path: String,
}

impl SshTarget {
    /// Round-trip string form suitable for both the `--ssh` argv and the
    /// `hosts.recent` pref.
    pub fn to_arg(&self) -> String {
        if self.path == "." {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.path)
        }
    }

    /// Inverse of `to_arg`.
    pub fn parse(s: &str) -> Self {
        match s.split_once(':') {
            Some((host, "")) => Self {
                host: host.to_string(),
                path: ".".to_string(),
            },
            Some((host, path)) => Self {
                host: host.to_string(),
                path: path.to_string(),
            },
            None => Self {
                host: s.to_string(),
                path: ".".to_string(),
            },
        }
    }
}

pub struct HostsPickerState {
    pub active: bool,
    pub all_hosts: Vec<HostEntry>,
    /// Recent targets as declared by the user in prior sessions, in
    /// most-recent-first order. Duplicates against `all_hosts` are
    /// rendered in the "recent" section and omitted from the main list.
    pub recent: Vec<SshTarget>,
    pub filter: String,
    pub selected_idx: usize,
    pub input_mode: InputMode,
    /// Raw buffer for the path input mode (typed `[user@]host[:path]`).
    pub path_buffer: String,
    pub last_popup_area: Option<Rect>,
}

impl Default for HostsPickerState {
    fn default() -> Self {
        Self {
            active: false,
            all_hosts: Vec::new(),
            recent: Vec::new(),
            filter: String::new(),
            selected_idx: 0,
            input_mode: InputMode::Search,
            path_buffer: String::new(),
            last_popup_area: None,
        }
    }
}

/// Visible row in the picker list, either a recent-target row or a
/// config-entry row. The renderer keeps the two groups in one flat list
/// so Up/Down navigation doesn't need a separate cursor per section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerRow {
    Recent(SshTarget),
    Entry(HostEntry),
}

impl HostsPickerState {
    /// Prep state from the parsed config + loaded recents and activate
    /// the overlay. Idempotent — calling it while already open just
    /// resets the filter and selection.
    pub fn open(&mut self, all_hosts: Vec<HostEntry>, recent: Vec<SshTarget>) {
        self.all_hosts = all_hosts;
        self.recent = recent;
        self.filter.clear();
        self.path_buffer.clear();
        self.selected_idx = 0;
        self.input_mode = InputMode::Search;
        self.active = true;
    }

    pub fn close(&mut self) {
        self.active = false;
        self.filter.clear();
        self.path_buffer.clear();
        self.input_mode = InputMode::Search;
        self.selected_idx = 0;
    }

    pub fn enter_path_mode(&mut self) {
        self.input_mode = InputMode::Path;
        // Seed the path buffer with whatever filter text the user had
        // typed so they don't lose it. Filter-as-prefix covers the
        // common "oh this alias isn't here, let me type it" path.
        if self.path_buffer.is_empty() {
            self.path_buffer = self.filter.clone();
        }
    }

    /// Apply the current filter against the cached parsed hosts. Recent
    /// targets always appear first; subsequent entries are anything
    /// whose alias / hostname contains the filter (case-insensitive).
    /// Entries already present in `recent` are skipped to avoid double-
    /// rendering.
    pub fn visible_rows(&self) -> Vec<PickerRow> {
        let mut out: Vec<PickerRow> = Vec::new();
        let f = self.filter.to_ascii_lowercase();

        let recent_aliases: std::collections::HashSet<String> = self
            .recent
            .iter()
            .map(|t| t.host.to_ascii_lowercase())
            .collect();

        for t in &self.recent {
            if f.is_empty()
                || t.host.to_ascii_lowercase().contains(&f)
                || t.path.to_ascii_lowercase().contains(&f)
            {
                out.push(PickerRow::Recent(t.clone()));
            }
        }
        for h in &self.all_hosts {
            if recent_aliases.contains(&h.alias.to_ascii_lowercase()) {
                continue;
            }
            if f.is_empty() || matches_entry(h, &f) {
                out.push(PickerRow::Entry(h.clone()));
            }
        }
        out
    }

    /// Materialise whichever row is currently selected into an
    /// `SshTarget`. Returns `None` in path-input mode when the buffer
    /// is empty, or in search mode when there's no visible row
    /// (filter matched nothing).
    pub fn confirm(&self) -> Option<SshTarget> {
        match self.input_mode {
            InputMode::Path => {
                let trimmed = self.path_buffer.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(SshTarget::parse(trimmed))
                }
            }
            InputMode::Search => {
                let rows = self.visible_rows();
                rows.get(self.selected_idx).map(|row| match row {
                    PickerRow::Recent(t) => t.clone(),
                    PickerRow::Entry(h) => SshTarget {
                        host: h.alias.clone(),
                        path: ".".to_string(),
                    },
                })
            }
        }
    }

    /// Bump the selection, clamped to the current row count. `delta` is
    /// signed so callers can use `+1` / `-1` without a separate method.
    pub fn move_selection(&mut self, delta: i32) {
        let rows = self.visible_rows();
        if rows.is_empty() {
            self.selected_idx = 0;
            return;
        }
        let last = rows.len() as i32 - 1;
        let next = (self.selected_idx as i32 + delta).clamp(0, last);
        self.selected_idx = next as usize;
    }
}

fn matches_entry(h: &HostEntry, filter_lower: &str) -> bool {
    h.alias.to_ascii_lowercase().contains(filter_lower)
        || h.hostname
            .as_deref()
            .map(|v| v.to_ascii_lowercase().contains(filter_lower))
            .unwrap_or(false)
        || h.user
            .as_deref()
            .map(|v| v.to_ascii_lowercase().contains(filter_lower))
            .unwrap_or(false)
}

/// Load the recent-targets list from prefs, cropped to `MAX_RECENT`.
pub fn load_recent() -> Vec<SshTarget> {
    let Some(raw) = crate::prefs::get(RECENT_PREF_KEY) else {
        return Vec::new();
    };
    raw.split('\t')
        .filter(|s| !s.is_empty())
        .take(MAX_RECENT)
        .map(SshTarget::parse)
        .collect()
}

/// Persist the recent list. Drops duplicates (most-recent wins) and
/// crops to `MAX_RECENT`.
pub fn save_recent(targets: &[SshTarget]) {
    let mut seen = std::collections::HashSet::new();
    let mut deduped: Vec<&SshTarget> = Vec::new();
    for t in targets {
        let key = t.to_arg();
        if seen.insert(key) {
            deduped.push(t);
        }
        if deduped.len() >= MAX_RECENT {
            break;
        }
    }
    let joined = deduped
        .iter()
        .map(|t| t.to_arg())
        .collect::<Vec<_>>()
        .join("\t");
    crate::prefs::set(RECENT_PREF_KEY, &joined);
}

/// Push `new` onto the head of an existing recent list, dedupe, crop.
/// Returns the new list so the caller can write it back to prefs.
pub fn bump_recent(mut recent: Vec<SshTarget>, new: SshTarget) -> Vec<SshTarget> {
    recent.retain(|t| t.to_arg() != new.to_arg());
    recent.insert(0, new);
    recent.truncate(MAX_RECENT);
    recent
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(alias: &str) -> HostEntry {
        HostEntry {
            alias: alias.to_string(),
            hostname: None,
            user: None,
        }
    }

    #[test]
    fn ssh_target_arg_roundtrip() {
        let t = SshTarget {
            host: "prod".into(),
            path: "/var/app".into(),
        };
        let s = t.to_arg();
        assert_eq!(s, "prod:/var/app");
        assert_eq!(SshTarget::parse(&s), t);
    }

    #[test]
    fn ssh_target_default_path_is_dot() {
        let t = SshTarget::parse("prod");
        assert_eq!(t.path, ".");
        assert_eq!(t.to_arg(), "prod");
    }

    #[test]
    fn filter_is_case_insensitive_substring() {
        let mut s = HostsPickerState::default();
        s.open(vec![h("ProdDb"), h("staging"), h("dev-a")], vec![]);
        s.filter = "db".into();
        let rows = s.visible_rows();
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0], PickerRow::Entry(ref e) if e.alias == "ProdDb"));
    }

    #[test]
    fn recent_entries_suppress_duplicates_in_main_list() {
        let mut s = HostsPickerState::default();
        s.open(
            vec![h("prod"), h("staging")],
            vec![SshTarget::parse("prod")],
        );
        let rows = s.visible_rows();
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0], PickerRow::Recent(_)));
        assert!(matches!(rows[1], PickerRow::Entry(ref e) if e.alias == "staging"));
    }

    #[test]
    fn confirm_in_path_mode_parses_host_with_path() {
        let mut s = HostsPickerState::default();
        s.open(vec![], vec![]);
        s.input_mode = InputMode::Path;
        s.path_buffer = "user@host:/srv/app".into();
        let t = s.confirm().unwrap();
        assert_eq!(t.host, "user@host");
        assert_eq!(t.path, "/srv/app");
    }

    #[test]
    fn confirm_in_path_mode_rejects_empty() {
        let mut s = HostsPickerState::default();
        s.open(vec![], vec![]);
        s.input_mode = InputMode::Path;
        assert!(s.confirm().is_none());
    }

    #[test]
    fn confirm_in_search_mode_returns_selected_entry() {
        let mut s = HostsPickerState::default();
        s.open(vec![h("one"), h("two")], vec![]);
        s.selected_idx = 1;
        let t = s.confirm().unwrap();
        assert_eq!(t.host, "two");
        assert_eq!(t.path, ".");
    }

    #[test]
    fn move_selection_clamps() {
        let mut s = HostsPickerState::default();
        s.open(vec![h("a"), h("b"), h("c")], vec![]);
        s.move_selection(10);
        assert_eq!(s.selected_idx, 2);
        s.move_selection(-99);
        assert_eq!(s.selected_idx, 0);
    }

    #[test]
    fn bump_recent_deduplicates_and_crops() {
        let one = SshTarget::parse("one");
        let two = SshTarget::parse("two");
        let recent = vec![one.clone(), two.clone()];
        let pushed = bump_recent(recent, one.clone());
        assert_eq!(pushed, vec![one, two]);
    }

    #[test]
    fn bump_recent_crops_to_max() {
        // Push MAX_RECENT+2 entries — earliest should drop off.
        let mut recent = Vec::new();
        for i in 0..(MAX_RECENT + 2) {
            recent = bump_recent(recent, SshTarget::parse(&format!("h{i}")));
        }
        assert_eq!(recent.len(), MAX_RECENT);
        // Newest first.
        assert_eq!(recent[0].host, format!("h{}", MAX_RECENT + 1));
    }
}
