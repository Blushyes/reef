//! Space-leader chord arming behaviour around text-input contexts.
//!
//! The leader is `bare Space` followed by `p` / `f`. The arm step must
//! NOT fire while the user is mid-typing in a text input — otherwise
//! the Space they meant as a literal separator vanishes into the chord
//! state machine. Two input contexts qualify: the Tab::Search query
//! and the Tab::Git commit box. Empty buffers stay armable (no char
//! to swallow yet).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reef::app::{App, Panel, Tab};
use reef::input;
use reef::ui::theme::Theme;
use std::sync::Mutex;
use tempfile::TempDir;
use test_support::CwdGuard;

static CWD_LOCK: Mutex<()> = Mutex::new(());

fn space_key() -> KeyEvent {
    KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)
}

fn s_key() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)
}

#[test]
fn bare_space_arms_leader_in_normal_context() {
    // Sanity check: when no text input is focused, bare Space arms the
    // leader. Guards against a regression in the input-mode gate
    // accidentally suppressing the normal path.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.active_panel = Panel::Diff;
    assert!(app.space_leader_at.is_none());
    input::handle_key(space_key(), &mut app);
    assert!(
        app.space_leader_at.is_some(),
        "bare Space should arm in normal context"
    );
}

#[test]
fn bare_space_arms_leader_on_files_tree_panel() {
    // Files+Files used to be a carve-out where Space toggled multi-
    // selection. Multi-select moved to `s`, so Space here now arms the
    // leader uniformly with every other non-input context.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    assert_eq!(app.active_tab, Tab::Files);
    assert_eq!(app.active_panel, Panel::Files);

    input::handle_key(space_key(), &mut app);

    assert!(
        app.space_leader_at.is_some(),
        "Space on Files+Files should arm the leader (multi-select moved to `s`)",
    );
    assert!(
        app.file_selection.is_empty(),
        "Space must not touch the multi-selection set anymore",
    );
}

#[test]
fn s_toggles_selection_on_files_tree_panel() {
    // The new multi-select keybinding: bare `s` on Files+Files toggles
    // the cursor row in/out of the selection set without arming the
    // leader chord.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "").unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    assert_eq!(app.active_tab, Tab::Files);
    assert_eq!(app.active_panel, Panel::Files);
    assert!(app.file_selection.is_empty());

    input::handle_key(s_key(), &mut app);

    assert!(app.space_leader_at.is_none(), "`s` must not arm the leader",);
    if app.file_tree.selected_path().is_some() {
        assert_eq!(
            app.file_selection.len(),
            1,
            "`s` should have toggled the cursor into the selection",
        );
    }
}

#[test]
fn space_does_not_arm_while_typing_in_commit_box() {
    // Git tab + commit_editing=true + commit_message non-empty: the
    // user is mid-message ("fix: " ...), and bare Space must stay a
    // literal char rather than priming a chord that would swallow
    // the next `p`/`f` they type.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    app.active_panel = Panel::Files;
    app.git_status.commit_editing = true;
    app.git_status.commit_message = "fix:".to_string();

    input::handle_key(space_key(), &mut app);
    assert!(
        app.space_leader_at.is_none(),
        "Space inside a non-empty commit message must not arm the chord",
    );
}

#[test]
fn space_arms_when_commit_box_empty() {
    // Empty buffer is the arming-friendly edge case: there's no char
    // to clobber, and a chord that immediately fires is just as
    // useful here as anywhere else. Mirrors the global-search query
    // gate's `query.is_empty()` branch.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    app.active_panel = Panel::Files;
    app.git_status.commit_editing = true;
    app.git_status.commit_message.clear();

    input::handle_key(space_key(), &mut app);
    assert!(
        app.space_leader_at.is_some(),
        "Space with empty commit buffer should still arm",
    );
}

#[test]
fn space_does_not_arm_while_typing_in_search_query() {
    // Mirror test for the older Tab::Search input mode — the same
    // gate covers it, so a regression in either branch fails here.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Search);
    app.active_panel = Panel::Files;
    app.global_search.tab_input_focused = true;
    app.global_search.query = "foo".to_string();

    input::handle_key(space_key(), &mut app);
    assert!(
        app.space_leader_at.is_none(),
        "Space inside a non-empty search query must not arm the chord",
    );
}
