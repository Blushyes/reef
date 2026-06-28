use reef_core::nav::NavLang;
use reef_core::prefs::ThemePref;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingSection {
    General,
    Editor,
    Git,
    Graph,
    Nav,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingItem {
    Theme,
    EditorCommand,
    DiffLayout,
    DiffMode,
    StatusTreeMode,
    CommitDiffLayout,
    CommitDiffMode,
    CommitFilesTreeMode,
    Lsp(NavLang),
}

impl SettingItem {
    pub const ALL: &'static [SettingItem] = &[
        SettingItem::Theme,
        SettingItem::EditorCommand,
        SettingItem::DiffLayout,
        SettingItem::DiffMode,
        SettingItem::StatusTreeMode,
        SettingItem::CommitDiffLayout,
        SettingItem::CommitDiffMode,
        SettingItem::CommitFilesTreeMode,
        SettingItem::Lsp(NavLang::Rust),
        SettingItem::Lsp(NavLang::TypeScript),
        SettingItem::Lsp(NavLang::Tsx),
        SettingItem::Lsp(NavLang::Python),
        SettingItem::Lsp(NavLang::Go),
        SettingItem::Lsp(NavLang::Vue),
    ];

    pub fn section(self) -> SettingSection {
        match self {
            SettingItem::Theme => SettingSection::General,
            SettingItem::EditorCommand => SettingSection::Editor,
            SettingItem::DiffLayout | SettingItem::DiffMode | SettingItem::StatusTreeMode => {
                SettingSection::Git
            }
            SettingItem::CommitDiffLayout
            | SettingItem::CommitDiffMode
            | SettingItem::CommitFilesTreeMode => SettingSection::Graph,
            SettingItem::Lsp(_) => SettingSection::Nav,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditorEdit {
    pub buffer: String,
    pub cursor: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SettingsState {
    pub selected_idx: usize,
    pub editor_edit: Option<EditorEdit>,
    pub theme_pref: ThemePref,
    pub editor_command: String,
}

impl SettingsState {
    pub fn selected(&self) -> SettingItem {
        let i = self.selected_idx.min(SettingItem::ALL.len() - 1);
        SettingItem::ALL[i]
    }

    pub fn move_selection(&mut self, delta: i32) {
        let n = SettingItem::ALL.len() as i32;
        let cur = (self.selected_idx as i32).clamp(0, n - 1);
        let next = (cur + delta).rem_euclid(n);
        self.selected_idx = next as usize;
    }

    pub fn select(&mut self, idx: usize) {
        if idx < SettingItem::ALL.len() {
            self.selected_idx = idx;
        }
    }

    pub fn set_pref_cache(&mut self, theme_pref: ThemePref, editor_command: String) {
        self.theme_pref = theme_pref;
        self.editor_command = editor_command;
    }

    pub fn begin_edit_editor_command(&mut self) {
        let current = self.editor_command.clone();
        let cursor = current.len();
        self.editor_edit = Some(EditorEdit {
            buffer: current,
            cursor,
        });
    }

    pub fn cancel_editor_command(&mut self) {
        self.editor_edit = None;
    }

    pub fn commit_editor_command(&mut self) -> Option<String> {
        let edit = self.editor_edit.take()?;
        let sanitized = sanitize_editor_command(&edit.buffer);
        self.editor_command = sanitized.clone();
        Some(sanitized)
    }
}

pub fn sanitize_editor_command(raw: &str) -> String {
    raw.replace(['\n', '\r', '\t'], " ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_selection_wraps() {
        let mut s = SettingsState::default();
        s.move_selection(-1);
        assert_eq!(s.selected_idx, SettingItem::ALL.len() - 1);
        s.move_selection(1);
        assert_eq!(s.selected_idx, 0);
    }

    #[test]
    fn move_selection_clamps_starting_idx() {
        let mut s = SettingsState {
            selected_idx: 999,
            ..Default::default()
        };
        s.move_selection(1);
        assert_eq!(s.selected_idx, 0);
    }

    #[test]
    fn settings_section_assignment_is_complete() {
        for item in SettingItem::ALL {
            let _ = item.section();
        }
    }

    #[test]
    fn commit_editor_command_empty_clears_value() {
        let mut state = SettingsState {
            editor_edit: Some(EditorEdit {
                buffer: "   \t  ".to_string(),
                cursor: 0,
            }),
            ..Default::default()
        };
        let saved = state.commit_editor_command();
        assert_eq!(saved.as_deref(), Some(""));
        assert!(state.editor_edit.is_none());
        assert_eq!(state.editor_command, "");
    }

    #[test]
    fn commit_editor_command_sanitises_control_chars() {
        assert_eq!(
            sanitize_editor_command("code\n--wait\tfoo"),
            "code --wait foo"
        );
    }

    #[test]
    fn cancel_editor_command_keeps_value() {
        let mut state = SettingsState {
            editor_command: "vim".to_string(),
            editor_edit: Some(EditorEdit {
                buffer: "garbage".to_string(),
                cursor: 0,
            }),
            ..Default::default()
        };
        state.cancel_editor_command();
        assert!(state.editor_edit.is_none());
        assert_eq!(state.editor_command, "vim");
    }
}
