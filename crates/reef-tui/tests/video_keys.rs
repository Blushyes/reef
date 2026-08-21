//! Keyboard and mouse wiring for inline video playback, driven through the
//! real dispatchers against a real clip.
//!
//! The player itself is covered in `video_playback.rs`; what these check is
//! the path an interaction takes to reach it — which panel has to hold focus
//! for `p`, and that the button remains available from tree focus.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui_image::picker::{Picker, ProtocolType};
use reef::TuiApp as App;
use reef::input;
use reef::ui;
use reef::ui::mouse::ClickAction;
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
fn write_clip(dir: &Path, seconds: u32) -> bool {
    Command::new("ffmpeg")
        .arg("-y")
        .args(["-f", "lavfi"])
        .args([
            "-i",
            &format!("testsrc2=size=320x240:rate=15:duration={seconds}"),
        ])
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
    panic!(
        "timed out waiting for {label}; video={:?}, status={:?}",
        app.video.as_ref().map(|player| (
            player.position(),
            player.is_playing(),
            player.has_ended()
        )),
        app.video_status
    );
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
    app_previewing_a_clip_for(4)
}

fn app_previewing_a_clip_for(seconds: u32) -> Option<Fixture> {
    force_en_lang();
    let (tmp, _raw) = tempdir_repo();
    if !write_clip(tmp.path(), seconds) {
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
fn transport_button_toggles_playback_from_tree_focus() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut fixture) = app_previewing_a_clip() else {
        return;
    };
    let app = &mut fixture.app;
    assert_eq!(app.engine.active_panel(), reef_app::AppPanel::Files);

    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|frame| ui::render(frame, app)).unwrap();
    let button = (0..30)
        .flat_map(|row| (0..120).map(move |column| (column, row)))
        .find(|&(column, row)| {
            matches!(
                app.hit_registry.hit_test(column, row),
                Some(ClickAction::ToggleVideoPlayback)
            )
        })
        .expect("rendered video transport button");

    input::handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: button.0,
            row: button.1,
            modifiers: KeyModifiers::NONE,
        },
        app,
        &terminal,
    );

    assert!(
        app.video.as_ref().unwrap().is_playing(),
        "the button plays even while keyboard focus starts on the tree"
    );

    input::handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: button.0,
            row: button.1,
            modifiers: KeyModifiers::NONE,
        },
        app,
        &terminal,
    );
    assert!(
        !app.video.as_ref().unwrap().is_playing(),
        "clicking the transport button again pauses playback"
    );
}

#[test]
fn dragging_the_timeline_seeks_on_release() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut fixture) = app_previewing_a_clip() else {
        return;
    };
    let app = &mut fixture.app;
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|frame| ui::render(frame, app)).unwrap();

    let (start, width, row) = (0..30)
        .flat_map(|row| (0..120).map(move |column| (column, row)))
        .find_map(
            |(column, row)| match app.hit_registry.hit_test(column, row) {
                Some(ClickAction::SeekVideo { start, width }) => Some((start, width, row)),
                _ => None,
            },
        )
        .expect("rendered video timeline");
    let target = start + width.saturating_sub(1) * 3 / 4;

    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        input::handle_mouse(
            MouseEvent {
                kind,
                column: target,
                row,
                modifiers: KeyModifiers::NONE,
            },
            app,
            &terminal,
        );
    }

    wait_until(app, "timeline seek", |app| {
        app.video
            .as_ref()
            .is_some_and(|player| player.position() > 2.5)
    });
    assert!(
        !app.video.as_ref().unwrap().is_playing(),
        "a paused clip stays paused after seeking"
    );
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
fn engine_driven_tab_change_pauses_playback() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut fixture) = app_previewing_a_clip() else {
        return;
    };
    let app = &mut fixture.app;

    app.cycle_active_panel(false);
    input::handle_key(key(KeyCode::Char('p')), app);
    assert!(app.video.as_ref().unwrap().is_playing());

    app.engine
        .dispatch(reef_app::AppCommand::SetActiveTab(reef_app::AppTab::Git));
    app.tick();
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
    fixture.app.cycle_active_panel(false);
    input::handle_key(key(KeyCode::Char('p')), &mut fixture.app);
    assert!(fixture.app.video.as_ref().unwrap().is_playing());

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
    assert!(
        !app.video.as_ref().unwrap().is_playing(),
        "a stale player pauses as soon as another preview is requested"
    );
    wait_until(app, "text preview", |app| {
        !app.engine.state.preview_load.loading && app.video.is_none()
    });

    assert!(!app.preview_is_video());
    assert!(app.video_status.is_none());
}

#[test]
fn replay_request_returns_before_decoder_reopens() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut fixture) = app_previewing_a_clip_for(1) else {
        return;
    };
    let app = &mut fixture.app;
    app.cycle_active_panel(false);
    input::handle_key(key(KeyCode::Char('p')), app);
    wait_until(app, "video end", |app| {
        app.video.as_ref().is_some_and(|player| player.has_ended())
    });

    let started = Instant::now();
    input::handle_key(key(KeyCode::Char('p')), app);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the input handler must only enqueue replay work"
    );
    wait_until(app, "replay", |app| {
        app.video
            .as_ref()
            .is_some_and(|player| player.is_playing() && !player.has_ended())
    });
}
