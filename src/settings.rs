//! Settings page state + pure helpers. The renderer lives in
//! `ui::settings_panel`; the keyboard router in `input::handle_key_settings`.
//! Adding a new row is one [`SettingItem`] variant + an entry in
//! [`SettingItem::ALL`] + match arms in `section`/`label`/`description`/
//! [`current_value`]/[`cycle`].

use crate::app::{App, DiffLayout, DiffMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingSection {
    General,
    Editor,
    Git,
    Graph,
}

impl SettingSection {
    pub(crate) fn label(self) -> crate::i18n::Msg {
        use crate::i18n::Msg;
        match self {
            SettingSection::General => Msg::SettingsSectionGeneral,
            SettingSection::Editor => Msg::SettingsSectionEditor,
            SettingSection::Git => Msg::SettingsSectionGit,
            SettingSection::Graph => Msg::SettingsSectionGraph,
        }
    }
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
    ];

    pub(crate) fn section(self) -> SettingSection {
        match self {
            SettingItem::Theme => SettingSection::General,
            SettingItem::EditorCommand => SettingSection::Editor,
            SettingItem::DiffLayout | SettingItem::DiffMode | SettingItem::StatusTreeMode => {
                SettingSection::Git
            }
            SettingItem::CommitDiffLayout
            | SettingItem::CommitDiffMode
            | SettingItem::CommitFilesTreeMode => SettingSection::Graph,
        }
    }

    pub(crate) fn label(self) -> crate::i18n::Msg {
        use crate::i18n::Msg;
        match self {
            SettingItem::Theme => Msg::SettingsItemTheme,
            SettingItem::EditorCommand => Msg::SettingsItemEditor,
            SettingItem::DiffLayout => Msg::SettingsItemDiffLayout,
            SettingItem::DiffMode => Msg::SettingsItemDiffMode,
            SettingItem::StatusTreeMode => Msg::SettingsItemStatusTreeMode,
            SettingItem::CommitDiffLayout => Msg::SettingsItemCommitDiffLayout,
            SettingItem::CommitDiffMode => Msg::SettingsItemCommitDiffMode,
            SettingItem::CommitFilesTreeMode => Msg::SettingsItemCommitFilesTreeMode,
        }
    }

    pub(crate) fn description(self) -> crate::i18n::Msg {
        use crate::i18n::Msg;
        match self {
            SettingItem::Theme => Msg::SettingsDescTheme,
            SettingItem::EditorCommand => Msg::SettingsDescEditor,
            SettingItem::DiffLayout => Msg::SettingsDescDiffLayout,
            SettingItem::DiffMode => Msg::SettingsDescDiffMode,
            SettingItem::StatusTreeMode => Msg::SettingsDescStatusTreeMode,
            SettingItem::CommitDiffLayout => Msg::SettingsDescCommitDiffLayout,
            SettingItem::CommitDiffMode => Msg::SettingsDescCommitDiffMode,
            SettingItem::CommitFilesTreeMode => Msg::SettingsDescCommitFilesTreeMode,
        }
    }
}

/// Theme preference — distinct from `ui::theme::Theme` (the resolved
/// palette) because `Auto` isn't a palette, it's a "probe at startup"
/// instruction. The render path needs the pref string for display; the
/// cycle path needs the next state. Keeping it as one enum centralises
/// both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemePref {
    #[default]
    Auto,
    Dark,
    Light,
}

impl ThemePref {
    fn pref_str(self) -> &'static str {
        match self {
            ThemePref::Auto => "auto",
            ThemePref::Dark => "dark",
            ThemePref::Light => "light",
        }
    }

    fn from_pref_str(s: &str) -> Self {
        match s {
            "dark" => ThemePref::Dark,
            "light" => ThemePref::Light,
            _ => ThemePref::Auto,
        }
    }

    fn next(self) -> Self {
        match self {
            ThemePref::Auto => ThemePref::Dark,
            ThemePref::Dark => ThemePref::Light,
            ThemePref::Light => ThemePref::Auto,
        }
    }

    fn label_msg(self) -> crate::i18n::Msg {
        use crate::i18n::Msg;
        match self {
            ThemePref::Auto => Msg::SettingsValueThemeAuto,
            ThemePref::Dark => Msg::SettingsValueThemeDark,
            ThemePref::Light => Msg::SettingsValueThemeLight,
        }
    }
}

#[derive(Debug, Default)]
pub struct EditorEdit {
    pub buffer: String,
    pub cursor: usize,
}

#[derive(Debug, Default)]
pub struct SettingsState {
    pub selected_idx: usize,
    pub editor_edit: Option<EditorEdit>,
    /// Pref-backed values cached so [`current_value`] reads memory, not
    /// disk. Refreshed by [`refresh_pref_cache`] on every entry to the
    /// page and after every mutation.
    theme_pref: ThemePref,
    editor_command: String,
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
}

/// Reload the pref-backed cache from disk. Cheap (one prefs read) but
/// not free, so we only call this on transitions: opening the page, and
/// after each mutation that could have changed a cached value.
pub fn refresh_pref_cache(state: &mut SettingsState) {
    state.theme_pref = crate::prefs::get("ui.theme")
        .as_deref()
        .map(ThemePref::from_pref_str)
        .unwrap_or_default();
    state.editor_command = crate::prefs::get("editor.command").unwrap_or_default();
}

#[derive(Debug, Clone)]
pub enum ItemValue {
    Bool(bool),
    Choice(&'static str),
    Text(String),
}

pub(crate) fn current_value(item: SettingItem, app: &App) -> ItemValue {
    use crate::i18n::t;
    match item {
        SettingItem::Theme => ItemValue::Choice(t(app.settings.theme_pref.label_msg())),
        SettingItem::EditorCommand => ItemValue::Text(app.settings.editor_command.clone()),
        SettingItem::DiffLayout => ItemValue::Choice(diff_layout_label(app.diff_layout)),
        SettingItem::DiffMode => ItemValue::Choice(diff_mode_label(app.diff_mode)),
        SettingItem::StatusTreeMode => ItemValue::Bool(app.git_status.tree_mode),
        SettingItem::CommitDiffLayout => {
            ItemValue::Choice(diff_layout_label(app.commit_detail.diff_layout))
        }
        SettingItem::CommitDiffMode => {
            ItemValue::Choice(diff_mode_label(app.commit_detail.diff_mode))
        }
        SettingItem::CommitFilesTreeMode => ItemValue::Bool(app.commit_detail.files_tree_mode),
    }
}

fn diff_layout_label(layout: DiffLayout) -> &'static str {
    use crate::i18n::{Msg, t};
    match layout {
        DiffLayout::Unified => t(Msg::LayoutUnified),
        DiffLayout::SideBySide => t(Msg::LayoutSideBySide),
    }
}

fn diff_mode_label(mode: DiffMode) -> &'static str {
    use crate::i18n::{Msg, t};
    match mode {
        DiffMode::Compact => t(Msg::ModeCompact),
        DiffMode::FullFile => t(Msg::ModeFullFile),
    }
}

/// Cycle the selected item. `EditorCommand` is a no-op here — the input
/// layer routes Enter on that row to [`begin_edit_editor_command`] instead.
pub fn cycle(app: &mut App, item: SettingItem) {
    match item {
        SettingItem::Theme => cycle_theme(app),
        SettingItem::EditorCommand => {}
        SettingItem::DiffLayout => app.toggle_diff_layout(),
        SettingItem::DiffMode => app.toggle_diff_mode(),
        SettingItem::StatusTreeMode => app.toggle_status_tree_mode(),
        SettingItem::CommitDiffLayout => app.toggle_commit_diff_layout(),
        SettingItem::CommitDiffMode => app.toggle_commit_diff_mode(),
        SettingItem::CommitFilesTreeMode => app.toggle_commit_files_tree_mode(),
    }
    refresh_pref_cache(&mut app.settings);
}

fn cycle_theme(app: &mut App) {
    let next = app.settings.theme_pref.next();
    crate::prefs::set("ui.theme", next.pref_str());
    match next {
        ThemePref::Dark => app.theme = crate::ui::theme::Theme::dark(),
        ThemePref::Light => app.theme = crate::ui::theme::Theme::light(),
        // The OSC 11 probe must run before raw mode, so we can't redo
        // it from here. Keep the current live theme and tell the user
        // their pick takes effect on next launch.
        ThemePref::Auto => {
            app.toasts
                .push(crate::ui::toast::Toast::info(crate::i18n::t(
                    crate::i18n::Msg::SettingsAutoThemeOnNextLaunch,
                )));
        }
    }
}

pub fn begin_edit_editor_command(state: &mut SettingsState) {
    let current = state.editor_command.clone();
    let cursor = current.len();
    state.editor_edit = Some(EditorEdit {
        buffer: current,
        cursor,
    });
}

/// Sanitises control characters before writing: the prefs file is a
/// flat `key=value` per line, so a stray `\n` would corrupt every key
/// after `editor.command`.
pub(crate) fn commit_editor_command(state: &mut SettingsState) {
    if let Some(edit) = state.editor_edit.take() {
        let trimmed = edit.buffer.replace(['\n', '\r', '\t'], " ");
        let trimmed = trimmed.trim();
        crate::prefs::set("editor.command", trimmed);
        state.editor_command = trimmed.to_string();
    }
}

pub(crate) fn cancel_editor_command(state: &mut SettingsState) {
    state.editor_edit = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::ui::theme::Theme;
    use std::sync::MutexGuard;
    use tempfile::TempDir;
    use test_support::{CwdGuard, HOME_LOCK, HomeGuard};

    fn isolated_app() -> (
        MutexGuard<'static, ()>,
        HomeGuard,
        CwdGuard,
        TempDir,
        TempDir,
        App,
    ) {
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let h = HomeGuard::enter(home.path());
        let g = CwdGuard::enter(cwd.path());
        let app = App::new(Theme::dark(), None);
        (lock, h, g, home, cwd, app)
    }

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
        // ALL shrinking between sessions could leave a stale idx in
        // saved state — next nav must recover, not panic.
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
            let _ = item.label();
            let _ = item.description();
        }
    }

    #[test]
    fn cycle_status_tree_mode_updates_app_and_prefs() {
        let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
        assert!(!app.git_status.tree_mode);

        cycle(&mut app, SettingItem::StatusTreeMode);
        assert!(app.git_status.tree_mode);
        assert_eq!(
            crate::prefs::get("status.tree_mode").as_deref(),
            Some("true")
        );

        cycle(&mut app, SettingItem::StatusTreeMode);
        assert!(!app.git_status.tree_mode);
        assert_eq!(
            crate::prefs::get("status.tree_mode").as_deref(),
            Some("false")
        );
    }

    #[test]
    fn cycle_theme_walks_auto_dark_light() {
        let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
        refresh_pref_cache(&mut app.settings);
        assert_eq!(app.settings.theme_pref, ThemePref::Auto);

        cycle(&mut app, SettingItem::Theme);
        assert_eq!(app.settings.theme_pref, ThemePref::Dark);
        assert!(app.theme.is_dark);

        cycle(&mut app, SettingItem::Theme);
        assert_eq!(app.settings.theme_pref, ThemePref::Light);
        assert!(!app.theme.is_dark);

        // Auto must NOT change the live theme — the probe ran before
        // raw mode and we can't redo it. Toast covers the visible-no-op.
        let toasts_before = app.toasts.len();
        cycle(&mut app, SettingItem::Theme);
        assert_eq!(app.settings.theme_pref, ThemePref::Auto);
        assert!(!app.theme.is_dark, "auto must not change live theme");
        assert!(app.toasts.len() > toasts_before);
    }

    #[test]
    fn cycle_commit_diff_layout_persists_independently() {
        // The Git tab and Graph tab keep separate prefs; crossing the
        // wires would silently break one tab.
        let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
        cycle(&mut app, SettingItem::CommitDiffLayout);
        assert!(matches!(
            app.commit_detail.diff_layout,
            DiffLayout::SideBySide
        ));
        assert_eq!(
            crate::prefs::get("commit.diff_layout").as_deref(),
            Some("side_by_side")
        );
        assert_eq!(crate::prefs::get("diff.layout"), None);
    }

    #[test]
    fn commit_editor_command_empty_clears_pref() {
        let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
        crate::prefs::set("editor.command", "old");
        app.settings.editor_edit = Some(EditorEdit {
            buffer: "   \t  ".to_string(),
            cursor: 0,
        });
        commit_editor_command(&mut app.settings);
        assert!(app.settings.editor_edit.is_none());
        assert_eq!(crate::prefs::get("editor.command").as_deref(), Some(""));
    }

    #[test]
    fn commit_editor_command_sanitises_control_chars() {
        let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
        app.settings.editor_edit = Some(EditorEdit {
            buffer: "code\n--wait\tfoo".to_string(),
            cursor: 0,
        });
        commit_editor_command(&mut app.settings);
        let saved = crate::prefs::get("editor.command").unwrap();
        assert!(!saved.contains('\n'));
        assert!(!saved.contains('\t'));
        assert!(!saved.contains('\r'));
        let (prog, _) = crate::editor::parse_editor_command(&saved).unwrap();
        assert_eq!(prog, "code");
    }

    #[test]
    fn cancel_editor_command_does_not_write_prefs() {
        let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
        crate::prefs::set("editor.command", "vim");
        app.settings.editor_edit = Some(EditorEdit {
            buffer: "garbage".to_string(),
            cursor: 0,
        });
        cancel_editor_command(&mut app.settings);
        assert!(app.settings.editor_edit.is_none());
        assert_eq!(crate::prefs::get("editor.command").as_deref(), Some("vim"));
    }

    #[test]
    fn open_settings_idempotent_preserves_edit() {
        let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
        app.open_settings();
        app.settings.editor_edit = Some(EditorEdit {
            buffer: "in progress".to_string(),
            cursor: 0,
        });
        app.open_settings();
        assert!(app.settings.editor_edit.is_some());
    }
}
