//! End-to-end checks for inline video playback against a real ffmpeg.
//!
//! These drive the actual decode pipeline — ffprobe, ffmpeg, the reader
//! thread, and protocol encoding — because everything interesting about
//! playback lives in the seams between those, not in the pure helpers the
//! unit tests cover. Environments without ffmpeg skip rather than fail: the
//! feature itself degrades to a still card there, so the test suite mirrors
//! that.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use ratatui_image::Image;
use ratatui_image::picker::{Picker, ProtocolType};
use reef::video::{VideoPlayer, VideoUnavailable};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// A picker that claims Kitty support with a known cell size, so frame
/// geometry is deterministic without a real terminal to query.
fn kitty_picker() -> Picker {
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize((10, 20));
    picker.set_protocol_type(ProtocolType::Kitty);
    picker
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Render a short synthetic clip. Returns `None` when ffmpeg can't be run,
/// which the callers treat as "skip".
fn fixture_clip(dir: &Path, seconds: u32, rate: u32) -> Option<PathBuf> {
    if !ffmpeg_available() {
        return None;
    }
    let path = dir.join("clip.mp4");
    let status = Command::new("ffmpeg")
        .arg("-y")
        .args(["-f", "lavfi"])
        .args([
            "-i",
            &format!("testsrc2=size=320x240:rate={rate}:duration={seconds}"),
        ])
        .args(["-pix_fmt", "yuv420p"])
        .arg(&path)
        .output()
        .ok()?;
    status.status.success().then_some(path)
}

/// Open a player, or skip the test when the environment can't support one.
fn open_player(path: &Path, picker: &Picker, area: Rect) -> Option<VideoPlayer> {
    match VideoPlayer::open(path, 0, picker, area) {
        Ok(player) => Some(player),
        // tmux and a missing ffmpeg are both "this environment doesn't do
        // inline video", not a defect in the pipeline.
        Err(VideoUnavailable::Tmux | VideoUnavailable::NoFfmpeg) => None,
        Err(other) => panic!("opening the fixture clip failed: {other:?}"),
    }
}

/// Drive `tick` on a synthetic clock until the player reports `frames` frame
/// changes, or the budget runs out. Returns how many frames actually landed.
fn advance(player: &mut VideoPlayer, picker: &Picker, frames: usize) -> usize {
    let start = Instant::now();
    let mut seen = 0;
    let mut clock = start;
    // The synthetic clock jumps a whole frame interval per step so the test
    // never waits on wall time, but the decoder still needs real time to
    // produce frames — hence the wall-clock budget as the escape hatch.
    while seen < frames && start.elapsed() < Duration::from_secs(20) {
        clock += Duration::from_millis(80);
        if player.tick(clock, picker) {
            seen += 1;
        }
    }
    seen
}

#[test]
fn opens_paused_on_the_first_frame() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(clip) = fixture_clip(dir.path(), 2, 15) else {
        return;
    };
    let picker = kitty_picker();
    let Some(player) = open_player(&clip, &picker, Rect::new(0, 0, 40, 12)) else {
        return;
    };

    assert!(
        !player.is_playing(),
        "a freshly opened clip waits for the user to start it"
    );
    assert!(!player.has_ended());
    assert!(
        player.frame().is_some(),
        "the opening frame is decoded before the card is first drawn"
    );

    let info = player.info();
    assert_eq!((info.width, info.height), (320, 240));
    assert_eq!(info.duration.map(|d| d.round()), Some(2.0));
}

#[test]
fn frames_land_within_the_panel_on_whole_cells() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(clip) = fixture_clip(dir.path(), 2, 15) else {
        return;
    };
    let picker = kitty_picker();
    let panel = Rect::new(4, 3, 40, 12);
    let Some(player) = open_player(&clip, &picker, panel) else {
        return;
    };

    let (_, area) = player.frame().expect("first frame");
    assert!(area.width <= panel.width && area.height <= panel.height);
    assert_eq!((area.x, area.y), (panel.x, panel.y));
    // 320×240 fitted into 400×240 px of panel keeps its 4:3 shape: 240 px
    // tall is 12 rows, and the matching width is 320 px — 32 columns.
    assert_eq!((area.width, area.height), (32, 12));
}

#[test]
fn playing_advances_frames_and_position() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(clip) = fixture_clip(dir.path(), 3, 15) else {
        return;
    };
    let picker = kitty_picker();
    let Some(mut player) = open_player(&clip, &picker, Rect::new(0, 0, 40, 12)) else {
        return;
    };

    assert_eq!(player.position(), 0.0);
    assert!(player.toggle());
    assert!(player.is_playing());

    let advanced = advance(&mut player, &picker, 5);
    assert_eq!(advanced, 5, "the decoder should keep up with 5 frames");
    assert!(
        player.position() > 0.0,
        "position tracks the frames consumed"
    );
    assert!(player.frame().is_some());
}

#[test]
fn a_paused_player_decodes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(clip) = fixture_clip(dir.path(), 2, 15) else {
        return;
    };
    let picker = kitty_picker();
    let Some(mut player) = open_player(&clip, &picker, Rect::new(0, 0, 40, 12)) else {
        return;
    };

    let before = player.position();
    assert_eq!(advance(&mut player, &picker, 1), 0);
    assert_eq!(player.position(), before);
}

#[test]
fn playback_ends_at_the_end_of_the_clip() {
    let dir = tempfile::tempdir().expect("tempdir");
    // One second at 5 fps is ~5 frames of playback to walk through.
    let Some(clip) = fixture_clip(dir.path(), 1, 5) else {
        return;
    };
    let picker = kitty_picker();
    let Some(mut player) = open_player(&clip, &picker, Rect::new(0, 0, 40, 12)) else {
        return;
    };

    assert!(player.toggle());
    // Ask for far more frames than the clip holds; the run stops early when
    // the stream ends.
    advance(&mut player, &picker, 60);

    assert!(player.has_ended(), "a clip that ran out reports as ended");
    assert!(!player.is_playing());
    assert!(
        player.frame().is_some(),
        "the last frame stays on screen after the stream ends"
    );

    // Replay is rebuilt off-thread by the adapter; exercise the same rebuild
    // primitive directly here, then resume it.
    player = player
        .rebuild(&picker, Rect::new(0, 0, 40, 12), 0.0)
        .expect("rebuild from the beginning");
    assert!(player.toggle());
    assert!(player.is_playing());
    assert!(!player.has_ended());
    assert_eq!(player.position(), 0.0);
}

#[test]
fn resizing_the_panel_keeps_playing_at_the_new_size() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(clip) = fixture_clip(dir.path(), 4, 15) else {
        return;
    };
    let picker = kitty_picker();
    let Some(mut player) = open_player(&clip, &picker, Rect::new(0, 0, 40, 12)) else {
        return;
    };

    assert!(player.toggle());
    advance(&mut player, &picker, 3);
    let position = player.position();
    let (_, before) = player.frame().expect("frame before resize");

    player = player
        .rebuild(&picker, Rect::new(0, 0, 20, 6), position)
        .expect("rebuild at the smaller size");
    assert!(player.toggle());

    // The card reports the new geometry immediately — the old frame is
    // stretched into it — and a fresh frame at that size follows shortly.
    assert_eq!(
        advance(&mut player, &picker, 1),
        1,
        "a frame at the new size"
    );
    let (_, after) = player.frame().expect("frame after resize");
    assert!(after.width < before.width && after.height < before.height);
    assert!(
        player.position() >= position,
        "playback resumes from where it was, not from the start"
    );

    // The re-started decoder keeps feeding frames.
    assert!(advance(&mut player, &picker, 2) > 0);
}

#[test]
fn matching_area_requires_no_rebuild() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(clip) = fixture_clip(dir.path(), 2, 15) else {
        return;
    };
    let picker = kitty_picker();
    let panel = Rect::new(0, 0, 40, 12);
    let Some(player) = open_player(&clip, &picker, panel) else {
        return;
    };

    assert!(
        player.matches_area(&picker, panel),
        "the same panel geometry must not require a decoder rebuild"
    );
}

#[test]
fn newer_source_revision_invalidates_same_path_player() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(clip) = fixture_clip(dir.path(), 2, 15) else {
        return;
    };
    let picker = kitty_picker();
    let player =
        VideoPlayer::open(&clip, 41, &picker, Rect::new(0, 0, 40, 12)).expect("open fixture clip");

    assert!(!player.matches_source(&clip, 42));
}

#[test]
fn a_file_that_is_not_a_video_is_reported_unreadable() {
    let dir = tempfile::tempdir().expect("tempdir");
    if !ffmpeg_available() {
        return;
    }
    let path = dir.path().join("not-a-video.mp4");
    std::fs::write(&path, b"this is not a container").expect("write fixture");

    match VideoPlayer::open(&path, 0, &kitty_picker(), Rect::new(0, 0, 40, 12)) {
        Err(VideoUnavailable::Unreadable(_)) => {}
        Err(VideoUnavailable::Tmux | VideoUnavailable::NoFfmpeg) => {}
        Err(other) => panic!("expected an unreadable verdict, got {other:?}"),
        Ok(_) => panic!("a file with no container should not open as a player"),
    }
}

#[test]
fn terminals_without_a_replaceable_protocol_do_not_play() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(clip) = fixture_clip(dir.path(), 1, 5) else {
        return;
    };
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize((10, 20));
    picker.set_protocol_type(ProtocolType::Sixel);

    assert!(matches!(
        VideoPlayer::open(&clip, 0, &picker, Rect::new(0, 0, 40, 12)),
        Err(VideoUnavailable::UnsupportedTerminal | VideoUnavailable::Tmux)
    ));
}

/// Render the player's current frame the way the preview card does, and
/// return the escape sequence the terminal would receive. Protocols write
/// themselves into a buffer cell's symbol, so that string *is* the wire
/// data.
fn wire_bytes(player: &VideoPlayer) -> String {
    let (protocol, area) = player.frame().expect("a frame to render");
    let mut buffer = Buffer::empty(Rect::new(0, 0, area.width.max(1), area.height.max(1)));
    let target = Rect::new(0, 0, area.width, area.height);
    Image::new(protocol).render(target, &mut buffer);
    buffer
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
}

#[test]
fn every_frame_transmits_fresh_pixels_to_the_terminal() {
    let dir = tempfile::tempdir().expect("tempdir");
    // testsrc2 animates, so consecutive frames genuinely differ.
    let Some(clip) = fixture_clip(dir.path(), 3, 15) else {
        return;
    };
    let picker = kitty_picker();
    let Some(mut player) = open_player(&clip, &picker, Rect::new(0, 0, 40, 12)) else {
        return;
    };

    let first = wire_bytes(&player);
    assert!(
        first.contains("\u{1b}_G"),
        "frames go out as Kitty graphics commands"
    );
    assert!(
        first.contains("a=T,U=1,f=32"),
        "each frame is a full RGBA transmit with a virtual placement"
    );
    assert!(
        first.contains("i=1380271616"),
        "frames reuse one image id so the terminal replaces rather than accumulates"
    );

    assert!(player.toggle());
    let mut frames = vec![first];
    for _ in 0..3 {
        assert_eq!(advance(&mut player, &picker, 1), 1, "another frame");
        frames.push(wire_bytes(&player));
    }

    // Consecutive frames must differ — identical payloads would mean the
    // card is re-sending one still image rather than playing.
    for pair in frames.windows(2) {
        assert_ne!(pair[0], pair[1], "each tick sends different pixels");
    }
    // And every frame is a real payload, not an empty placement.
    assert!(frames.iter().all(|frame| frame.len() > 10_000));
}

#[test]
fn a_paused_clip_rebuilds_at_the_new_size() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(clip) = fixture_clip(dir.path(), 3, 15) else {
        return;
    };
    let picker = kitty_picker();
    let Some(mut player) = open_player(&clip, &picker, Rect::new(0, 0, 40, 12)) else {
        return;
    };

    let before = wire_bytes(&player);
    assert!(!player.is_playing());
    player = player
        .rebuild(&picker, Rect::new(0, 0, 24, 8), player.position())
        .expect("rebuild paused clip");
    assert!(!player.is_playing(), "and stays paused");

    let after = wire_bytes(&player);
    assert_ne!(
        before, after,
        "the refreshed frame carries different pixels"
    );
    // 320×240 fitted into the 240×160 px the new panel offers is 213×160,
    // which rounds up to 22 columns of the 24 available.
    let (_, area) = player.frame().expect("frame");
    assert_eq!((area.width, area.height), (22, 8));
}
