//! Settings page entry / exit + inline-editor key dispatch.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reef::app::{App, Panel, Tab, ViewMode};
use reef::input;
use reef::settings::SettingItem;
use reef::ui::theme::Theme;
use std::sync::MutexGuard;
use tempfile::TempDir;
use test_support::{CwdGuard, HOME_LOCK, HomeGuard};

fn ctrl_comma() -> KeyEvent {
    KeyEvent::new(KeyCode::Char(','), KeyModifiers::CONTROL)
}

fn esc() -> KeyEvent {
    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
}

fn enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
}

fn char_key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

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

fn editor_command_idx() -> usize {
    SettingItem::ALL
        .iter()
        .position(|i| matches!(i, SettingItem::EditorCommand))
        .expect("EditorCommand must be in SettingItem::ALL")
}

#[test]
fn ctrl_comma_opens_settings_and_esc_closes() {
    let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
    assert_eq!(app.view_mode, ViewMode::Main);

    input::handle_key(ctrl_comma(), &mut app);
    assert_eq!(app.view_mode, ViewMode::Settings);

    input::handle_key(esc(), &mut app);
    assert_eq!(app.view_mode, ViewMode::Main);
}

#[test]
fn ctrl_comma_suppressed_in_commit_message_input_mode() {
    let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
    app.set_active_tab(Tab::Git);
    app.active_panel = Panel::Files;
    app.git_status.commit_editing = true;
    app.git_status.commit_message = "fix:".into();
    app.git_status.commit_cursor = app.git_status.commit_message.len();

    input::handle_key(ctrl_comma(), &mut app);
    assert_eq!(app.view_mode, ViewMode::Main);
}

#[test]
fn enter_on_editor_command_row_opens_inline_editor() {
    let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
    input::handle_key(ctrl_comma(), &mut app);
    app.settings.select(editor_command_idx());

    input::handle_key(enter(), &mut app);
    assert!(app.settings.editor_edit.is_some());

    for c in "nvim".chars() {
        input::handle_key(char_key(c), &mut app);
    }
    input::handle_key(enter(), &mut app);
    assert!(app.settings.editor_edit.is_none());
    assert_eq!(reef::prefs::get("editor.command").as_deref(), Some("nvim"));
}

#[test]
fn esc_in_inline_editor_cancels_without_writing_prefs() {
    let (_lock, _h, _g, _home, _cwd, mut app) = isolated_app();
    reef::prefs::set("editor.command", "vim");
    input::handle_key(ctrl_comma(), &mut app);
    app.settings.select(editor_command_idx());
    input::handle_key(enter(), &mut app);

    for c in "garbage".chars() {
        input::handle_key(char_key(c), &mut app);
    }
    // First Esc closes the inline editor; second Esc closes the page.
    input::handle_key(esc(), &mut app);
    assert!(app.settings.editor_edit.is_none());
    assert_eq!(app.view_mode, ViewMode::Settings);
    assert_eq!(reef::prefs::get("editor.command").as_deref(), Some("vim"));

    input::handle_key(esc(), &mut app);
    assert_eq!(app.view_mode, ViewMode::Main);
}
