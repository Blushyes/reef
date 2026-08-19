//! Keyboard wiring for inline video playback, driven through the real key
//! dispatcher against a real clip.
//!
//! The player itself is covered in `video_playback.rs`; what these check is
//! the path a keystroke takes to reach it — which panel has to hold focus,
//! and that `p` doesn't get eaten by the bindings it shares a letter with.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_image::picker::{Picker, ProtocolType};
use reef::TuiApp as App;
use reef::input;
use reef::ui::theme::Theme;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use test_support::{CwdGuard, HomeGuard, force_en_lang, tempdir_repo};

static CWD_LOCK: Mutex<()> = Mutex::new(());

fn kitty_picker() -> Picker {
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize((10, 20));
    picker.set_protocol_type(ProtocolType::Kitty);
    picker
}

/// Render a short clip into `dir`, or report that ffmpeg isn't available.
fn write_clip(dir: &Path) -> bool {
    Command::new("ffmpeg")
        .arg("-y")
        .args(["-f", "lavfi"])
        .args(["-i", "testsrc2=size=320x240:rate=15:duration=4"])
        .args(["-pix_fmt", "yuv420p"])
        .arg(dir.join("clip.mp4"))
        .output()
        .is_ok_and(|out| out.status.success())
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn wait_until(app: &mut App, label: &str, mut ready: impl FnMut(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        app.tick();
        if ready(app) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {label}");
}

/// An app previewing a clip, plus everything that has to outlive it: the
/// tempdirs the fixture lives in and the cwd / `$HOME` guards that must not
/// drop until the test is done.
struct Fixture {
    app: App,
    workdir: tempfile::TempDir,
    _home: tempfile::TempDir,
    _cwd_guard: CwdGuard,
    _home_guard: HomeGuard,
}

/// Bring up an app previewing a real video clip, with the player open and
/// paused on its first frame. Returns `None` when ffmpeg is unavailable.
fn app_previewing_a_clip() -> Option<Fixture> {
    force_en_lang();
    let (tmp, _raw) = tempdir_repo();
    if !write_clip(tmp.path()) {
        return None;
    }
    let home = tempfile::TempDir::new().expect("home tempdir");
    let home_guard = HomeGuard::enter(home.path());
    let cwd_guard = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), Some(kitty_picker()));
    app.refresh_file_tree();
    wait_until(&mut app, "file tree", |app| {
        !app.engine.state.file_tree_load.loading && !app.engine.state.file_tree_load.stale
    });
    let idx = app
        .engine
        .state
        .file_tree
        .entries
        .iter()
        .position(|e| e.name == "clip.mp4")
        .expect("clip.mp4 in tree");
    app.engine.state.file_tree.selected = idx;
    app.load_preview();
    wait_until(&mut app, "preview", |app| {
        !app.engine.state.preview_load.loading && app.engine.state.preview_content.is_some()
    });

    // The opener runs off-thread. In an environment that can't play inline
    // (no ffmpeg, or inside tmux) it reports a reason instead of a player,
    // and there is nothing here to test.
    wait_until(&mut app, "video opener", |app| {
        app.video.is_some() || app.video_status.is_some()
    });
    app.video.as_ref()?;

    Some(Fixture {
        app,
        workdir: tmp,
        _home: home,
        _cwd_guard: cwd_guard,
        _home_guard: home_guard,
    })
}

/// Pump ticks for a while so a playing clip can advance.
fn run_for(app: &mut App, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        app.tick();
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn p_plays_and_pauses_once_the_preview_panel_has_focus() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut fixture) = app_previewing_a_clip() else {
        return;
    };
    let app = &mut fixture.app;

    assert!(app.preview_is_video());
    assert!(!app.video.as_ref().unwrap().is_playing());

    // Tab moves focus from the tree to the preview, which is what unlocks
    // the bare `p` binding.
    app.cycle_active_panel(false);
    assert_eq!(app.engine.active_panel(), reef_app::AppPanel::Diff);

    input::handle_key(key(KeyCode::Char('p')), app);
    assert!(
        app.video.as_ref().unwrap().is_playing(),
        "p starts playback when the preview panel holds focus"
    );

    run_for(app, Duration::from_millis(600));
    assert!(
        app.video.as_ref().unwrap().position() > 0.0,
        "ticking a playing clip advances it"
    );

    input::handle_key(key(KeyCode::Char('p')), app);
    assert!(
        !app.video.as_ref().unwrap().is_playing(),
        "p pauses a playing clip"
    );
}

#[test]
fn p_on_the_tree_panel_leaves_playback_alone() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut fixture) = app_previewing_a_clip() else {
        return;
    };
    let app = &mut fixture.app;

    // Focus starts on the tree, where `p` is the paste binding.
    assert_eq!(app.engine.active_panel(), reef_app::AppPanel::Files);
    input::handle_key(key(KeyCode::Char('p')), app);
    assert!(
        !app.video.as_ref().unwrap().is_playing(),
        "p on the tree must not start playback"
    );
}

#[test]
fn leaving_the_files_tab_pauses_playback() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut fixture) = app_previewing_a_clip() else {
        return;
    };
    let app = &mut fixture.app;

    app.cycle_active_panel(false);
    input::handle_key(key(KeyCode::Char('p')), app);
    assert!(app.video.as_ref().unwrap().is_playing());

    app.set_active_tab(reef_app::AppTab::Git);
    assert!(
        !app.video.as_ref().unwrap().is_playing(),
        "a clip behind a hidden panel stops decoding"
    );
}

#[test]
fn selecting_another_file_tears_the_player_down() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut fixture) = app_previewing_a_clip() else {
        return;
    };
    std::fs::write(fixture.workdir.path().join("notes.txt"), "plain text\n")
        .expect("write sibling file");
    let app = &mut fixture.app;

    app.refresh_file_tree();
    wait_until(app, "file tree refresh", |app| {
        !app.engine.state.file_tree_load.loading && !app.engine.state.file_tree_load.stale
    });
    let idx = app
        .engine
        .state
        .file_tree
        .entries
        .iter()
        .position(|e| e.name == "notes.txt")
        .expect("notes.txt in tree");
    app.engine.state.file_tree.selected = idx;
    app.load_preview();
    wait_until(app, "text preview", |app| {
        !app.engine.state.preview_load.loading && app.video.is_none()
    });

    assert!(!app.preview_is_video());
    assert!(app.video_status.is_none());
}
