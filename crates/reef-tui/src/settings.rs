//! TUI settings labels, values, and persistence adapter.

use crate::TuiApp as App;
use reef_app::AppCommand;
use reef_app::DiffMode;
pub use reef_app::{SettingItem, SettingSection};
use reef_core::diff::DiffLayout;
use reef_core::nav::NavLang;
use reef_core::prefs::{EDITOR_COMMAND, ThemePref, UI_THEME};

pub(crate) fn section_label(section: SettingSection) -> crate::i18n::Msg {
    use crate::i18n::Msg;
    match section {
        SettingSection::General => Msg::SettingsSectionGeneral,
        SettingSection::Editor => Msg::SettingsSectionEditor,
        SettingSection::Git => Msg::SettingsSectionGit,
        SettingSection::Graph => Msg::SettingsSectionGraph,
        SettingSection::Nav => Msg::SettingsSectionNav,
    }
}

pub(crate) fn item_label(item: SettingItem) -> crate::i18n::Msg {
    use crate::i18n::Msg;
    match item {
        SettingItem::Theme => Msg::SettingsItemTheme,
        SettingItem::EditorCommand => Msg::SettingsItemEditor,
        SettingItem::DiffLayout => Msg::SettingsItemDiffLayout,
        SettingItem::DiffMode => Msg::SettingsItemDiffMode,
        SettingItem::StatusTreeMode => Msg::SettingsItemStatusTreeMode,
        SettingItem::CommitDiffLayout => Msg::SettingsItemCommitDiffLayout,
        SettingItem::CommitDiffMode => Msg::SettingsItemCommitDiffMode,
        SettingItem::CommitFilesTreeMode => Msg::SettingsItemCommitFilesTreeMode,
        SettingItem::Lsp(_) => Msg::SettingsItemLsp,
    }
}

pub(crate) fn item_description(item: SettingItem) -> crate::i18n::Msg {
    use crate::i18n::Msg;
    match item {
        SettingItem::Theme => Msg::SettingsDescTheme,
        SettingItem::EditorCommand => Msg::SettingsDescEditor,
        SettingItem::DiffLayout => Msg::SettingsDescDiffLayout,
        SettingItem::DiffMode => Msg::SettingsDescDiffMode,
        SettingItem::StatusTreeMode => Msg::SettingsDescStatusTreeMode,
        SettingItem::CommitDiffLayout => Msg::SettingsDescCommitDiffLayout,
        SettingItem::CommitDiffMode => Msg::SettingsDescCommitDiffMode,
        SettingItem::CommitFilesTreeMode => Msg::SettingsDescCommitFilesTreeMode,
        SettingItem::Lsp(_) => Msg::SettingsDescLsp,
    }
}

fn theme_label_msg(pref: ThemePref) -> crate::i18n::Msg {
    use crate::i18n::Msg;
    match pref {
        ThemePref::Auto => Msg::SettingsValueThemeAuto,
        ThemePref::Dark => Msg::SettingsValueThemeDark,
        ThemePref::Light => Msg::SettingsValueThemeLight,
    }
}

pub fn refresh_pref_cache(app: &mut App) {
    let theme_pref = crate::prefs::get(UI_THEME)
        .as_deref()
        .map(ThemePref::from_pref_str)
        .unwrap_or_default();
    let editor_command = crate::prefs::get(EDITOR_COMMAND).unwrap_or_default();
    app.engine.dispatch(AppCommand::SetSettingsPrefCache {
        theme_pref,
        editor_command,
    });
}

#[derive(Debug, Clone)]
pub enum ItemValue {
    Bool(bool),
    Choice(&'static str),
    Text(String),
    /// LSP row state — driven by `App::lsp_view_for(lang)`.
    LspStatus {
        lang: NavLang,
        state: LspRowState,
    },
}

/// Resolved render state for an LSP settings row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspRowState {
    /// Binary is on PATH AND supervisor has reported `Ready`. Both
    /// signals required because PATH-only means "installed but never
    /// asked" — still usable, but we hide the green dot until a real
    /// session lights it up.
    Ready,
    /// Binary on PATH but supervisor hasn't been spawned yet (no
    /// nav request for this language has fired).
    Available,
    /// Booting / Indexing.
    Booting,
    /// Last refine errored out — supervisor will respawn on the next
    /// request. Shown with a warning glyph.
    Crashed,
    /// Binary missing from PATH.
    Missing,
}

pub(crate) fn current_value(item: SettingItem, app: &App) -> ItemValue {
    use crate::i18n::t;
    match item {
        SettingItem::Theme => {
            ItemValue::Choice(t(theme_label_msg(app.engine.settings().theme_pref)))
        }
        SettingItem::EditorCommand => ItemValue::Text(app.engine.settings().editor_command.clone()),
        SettingItem::DiffLayout => ItemValue::Choice(diff_layout_label(app.engine.diff_layout())),
        SettingItem::DiffMode => ItemValue::Choice(diff_mode_label(app.engine.diff_mode())),
        SettingItem::StatusTreeMode => ItemValue::Bool(app.engine.status_tree_mode()),
        SettingItem::CommitDiffLayout => {
            ItemValue::Choice(diff_layout_label(app.engine.commit_diff_layout()))
        }
        SettingItem::CommitDiffMode => {
            ItemValue::Choice(diff_mode_label(app.engine.commit_diff_mode()))
        }
        SettingItem::CommitFilesTreeMode => ItemValue::Bool(app.engine.commit_files_tree_mode()),
        SettingItem::Lsp(lang) => ItemValue::LspStatus {
            lang,
            state: app.lsp_view_for(lang),
        },
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
        SettingItem::Lsp(lang) => {
            app.activate_lsp_row(lang);
            return;
        }
    }
    refresh_pref_cache(app);
}

fn cycle_theme(app: &mut App) {
    let next = app.engine.settings().theme_pref.next();
    crate::prefs::set(UI_THEME, next.pref_str());
    match next {
        ThemePref::Dark => app.theme = crate::ui::theme::Theme::dark(),
        ThemePref::Light => app.theme = crate::ui::theme::Theme::light(),
        // The OSC 11 probe must run before raw mode, so we can't redo
        // it from here. Keep the current live theme and tell the user
        // their pick takes effect on next launch.
        ThemePref::Auto => {
            app.push_toast(reef_app::Toast::info(crate::i18n::t(
                crate::i18n::Msg::SettingsAutoThemeOnNextLaunch,
            )));
        }
    }
}

pub(crate) fn commit_editor_command(app: &mut App) {
    let outcome = app
        .engine
        .dispatch(AppCommand::CommitSettingsEditorCommandEdit);
    if let Some(value) = outcome.committed_editor_command {
        crate::prefs::set(EDITOR_COMMAND, &value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TuiApp as App;
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
    fn settings_section_assignment_is_complete() {
        for item in SettingItem::ALL {
            let _ = item.section();
            let _ = item_label(*item);
            let _ = item_description(*item);
        }
    }

    #[test]
    fn cycle_status_tree_mode_updates_app_and_prefs() {
        let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
        assert!(!app.engine.state.git_status.tree_mode);

        cycle(&mut app, SettingItem::StatusTreeMode);
        assert!(app.engine.state.git_status.tree_mode);
        assert_eq!(
            crate::prefs::get("status.tree_mode").as_deref(),
            Some("true")
        );

        cycle(&mut app, SettingItem::StatusTreeMode);
        assert!(!app.engine.state.git_status.tree_mode);
        assert_eq!(
            crate::prefs::get("status.tree_mode").as_deref(),
            Some("false")
        );
    }

    #[test]
    fn cycle_theme_walks_auto_dark_light() {
        let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
        refresh_pref_cache(&mut app);
        assert_eq!(app.engine.settings().theme_pref, ThemePref::Auto);

        cycle(&mut app, SettingItem::Theme);
        assert_eq!(app.engine.settings().theme_pref, ThemePref::Dark);
        assert!(app.theme.is_dark);

        cycle(&mut app, SettingItem::Theme);
        assert_eq!(app.engine.settings().theme_pref, ThemePref::Light);
        assert!(!app.theme.is_dark);

        // Auto must NOT change the live theme — the probe ran before
        // raw mode and we can't redo it. Toast covers the visible-no-op.
        let toasts_before = app.engine.state.toasts.len();
        cycle(&mut app, SettingItem::Theme);
        assert_eq!(app.engine.settings().theme_pref, ThemePref::Auto);
        assert!(!app.theme.is_dark, "auto must not change live theme");
        assert!(app.engine.state.toasts.len() > toasts_before);
    }

    #[test]
    fn cycle_commit_diff_layout_persists_independently() {
        // The Git tab and Graph tab keep separate prefs; crossing the
        // wires would silently break one tab.
        let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
        cycle(&mut app, SettingItem::CommitDiffLayout);
        assert!(matches!(
            app.engine.state.commit_detail.diff_layout,
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
        app.engine.dispatch(AppCommand::SetSettingsPrefCache {
            theme_pref: ThemePref::Auto,
            editor_command: String::new(),
        });
        app.engine
            .dispatch(AppCommand::BeginSettingsEditorCommandEdit);
        app.engine.dispatch(AppCommand::PasteSettingsEditorCommand(
            "   \t  ".to_string(),
        ));
        commit_editor_command(&mut app);
        assert!(app.engine.settings().editor_edit.is_none());
        assert_eq!(crate::prefs::get("editor.command").as_deref(), Some(""));
    }

    #[test]
    fn commit_editor_command_persists_single_line_value_without_control_chars() {
        let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
        app.engine
            .dispatch(AppCommand::BeginSettingsEditorCommandEdit);
        app.engine.dispatch(AppCommand::PasteSettingsEditorCommand(
            "code\n--wait\tfoo".to_string(),
        ));
        commit_editor_command(&mut app);
        let saved = crate::prefs::get("editor.command").unwrap();
        assert!(!saved.contains('\n'));
        assert!(!saved.contains('\t'));
        assert!(!saved.contains('\r'));
        assert_eq!(saved, "code--wait foo");
    }

    #[test]
    fn cancel_editor_command_does_not_write_prefs() {
        let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
        crate::prefs::set("editor.command", "vim");
        app.engine.dispatch(AppCommand::SetSettingsPrefCache {
            theme_pref: ThemePref::Auto,
            editor_command: "vim".to_string(),
        });
        app.engine
            .dispatch(AppCommand::BeginSettingsEditorCommandEdit);
        app.engine.dispatch(AppCommand::PasteSettingsEditorCommand(
            "garbage".to_string(),
        ));
        app.engine
            .dispatch(AppCommand::CancelSettingsEditorCommandEdit);
        assert!(app.engine.settings().editor_edit.is_none());
        assert_eq!(crate::prefs::get("editor.command").as_deref(), Some("vim"));
    }

    #[test]
    fn open_settings_idempotent_preserves_edit() {
        let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
        app.open_settings();
        app.engine
            .dispatch(AppCommand::BeginSettingsEditorCommandEdit);
        app.open_settings();
        assert!(app.engine.settings().editor_edit.is_some());
    }
}
