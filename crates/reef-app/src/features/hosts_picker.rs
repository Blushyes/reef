use crate::PickerState;
use reef_core::hosts::HostEntry;

pub const MAX_RECENT: usize = 5;
pub const RECENT_PREF_KEY: &str = "hosts.recent";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Search,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    pub path: String,
}

impl SshTarget {
    pub fn to_arg(&self) -> String {
        if self.path == "." {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.path)
        }
    }

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
    pub core: PickerState,
    pub all_hosts: Vec<HostEntry>,
    pub recent: Vec<SshTarget>,
    pub input_mode: InputMode,
    pub path_buffer: String,
    pub path_cursor: usize,
}

impl Default for HostsPickerState {
    fn default() -> Self {
        Self {
            core: PickerState::default(),
            all_hosts: Vec::new(),
            recent: Vec::new(),
            input_mode: InputMode::Search,
            path_buffer: String::new(),
            path_cursor: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerRow {
    Recent(SshTarget),
    Entry(HostEntry),
}

impl HostsPickerState {
    pub fn open(&mut self, all_hosts: Vec<HostEntry>, recent: Vec<SshTarget>) {
        self.all_hosts = all_hosts;
        self.recent = recent;
        self.path_buffer.clear();
        self.path_cursor = 0;
        self.input_mode = InputMode::Search;
        self.core.open();
    }

    pub fn close(&mut self) {
        self.path_buffer.clear();
        self.path_cursor = 0;
        self.input_mode = InputMode::Search;
        self.core.close();
    }

    pub fn enter_path_mode(&mut self) {
        self.input_mode = InputMode::Path;
        if self.path_buffer.is_empty() {
            self.path_buffer = self.core.filter.clone();
            self.path_cursor = self.path_buffer.len();
        }
    }

    pub fn return_to_search_mode(&mut self) {
        self.input_mode = InputMode::Search;
        self.path_buffer.clear();
        self.path_cursor = 0;
    }

    pub fn handle_paste(&mut self, s: &str) {
        match self.input_mode {
            InputMode::Search => {
                if crate::text_input::paste_single_line(
                    s,
                    &mut self.core.filter,
                    &mut self.core.cursor,
                ) {
                    self.core.selected_idx = 0;
                }
            }
            InputMode::Path => {
                let _ = crate::text_input::paste_single_line(
                    s,
                    &mut self.path_buffer,
                    &mut self.path_cursor,
                );
            }
        }
    }

    pub fn visible_rows(&self) -> Vec<PickerRow> {
        let mut out: Vec<PickerRow> = Vec::new();
        let f = self.core.filter.to_ascii_lowercase();

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
                rows.get(self.core.selected_idx).map(|row| match row {
                    PickerRow::Recent(t) => t.clone(),
                    PickerRow::Entry(h) => SshTarget {
                        host: h.alias.clone(),
                        path: ".".to_string(),
                    },
                })
            }
        }
    }

    pub fn move_selection(&mut self, delta: i32) {
        let visible_count = self.visible_rows().len();
        self.core.move_selection(visible_count, delta);
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
        s.core.filter = "db".into();
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
        s.core.selected_idx = 1;
        let t = s.confirm().unwrap();
        assert_eq!(t.host, "two");
        assert_eq!(t.path, ".");
    }

    #[test]
    fn move_selection_clamps() {
        let mut s = HostsPickerState::default();
        s.open(vec![h("a"), h("b"), h("c")], vec![]);
        s.move_selection(10);
        assert_eq!(s.core.selected_idx, 2);
        s.move_selection(-99);
        assert_eq!(s.core.selected_idx, 0);
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
    fn handle_paste_routes_to_filter_in_search_mode() {
        let mut s = HostsPickerState::default();
        s.open(vec![h("prod")], vec![]);
        s.core.selected_idx = 7;
        s.handle_paste("staging");
        assert_eq!(s.core.filter, "staging");
        assert_eq!(s.core.cursor, 7);
        assert_eq!(s.core.selected_idx, 0);
        assert!(s.path_buffer.is_empty());
    }

    #[test]
    fn handle_paste_routes_to_path_buffer_in_path_mode() {
        let mut s = HostsPickerState::default();
        s.open(vec![], vec![]);
        s.input_mode = InputMode::Path;
        s.handle_paste("user@host:/srv");
        assert_eq!(s.path_buffer, "user@host:/srv");
        assert_eq!(s.path_cursor, "user@host:/srv".len());
        assert!(s.core.filter.is_empty());
    }

    #[test]
    fn handle_paste_strips_crlf_in_both_modes() {
        let mut s = HostsPickerState::default();
        s.open(vec![], vec![]);
        s.handle_paste("ab\r\ncd");
        assert_eq!(s.core.filter, "abcd");
        s.input_mode = InputMode::Path;
        s.handle_paste("xy\nzw");
        assert_eq!(s.path_buffer, "xyzw");
    }

    #[test]
    fn bump_recent_crops_to_max() {
        let mut recent = Vec::new();
        for i in 0..(MAX_RECENT + 2) {
            recent = bump_recent(recent, SshTarget::parse(&format!("h{i}")));
        }
        assert_eq!(recent.len(), MAX_RECENT);
        assert_eq!(recent[0].host, format!("h{}", MAX_RECENT + 1));
    }
}
