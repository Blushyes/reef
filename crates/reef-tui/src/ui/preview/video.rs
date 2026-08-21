//! Video preview card: source metadata, the current frame, and a transport
//! line showing playback position.
//!
//! The card is the same shape whether or not the clip can play inline. A
//! terminal without a usable graphics protocol, a missing ffmpeg, or a file
//! on the remote side of an SSH session all render the identical card with
//! the reason where the frame would be — so a still card never reads as a
//! failure to load.

use crate::TuiApp as App;
use crate::i18n::{Msg, t};
use crate::ui::preview::chrome::render_card_header;
use crate::video::{VideoUnavailable, format_timecode};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui_image::Image;
use reef_core::preview::BinaryInfo;
use unicode_width::UnicodeWidthStr;

/// Below this height the card drops the metadata line and the transport line
/// and gives every row to the frame.
const MIN_CHROME_HEIGHT: u16 = 8;

pub(in crate::ui) fn render(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    path: &str,
    info: &BinaryInfo,
    focused: bool,
) {
    if area.height < 1 {
        return;
    }
    let th = app.theme;
    let max_y = area.y + area.height;
    let mut y = render_card_header(f, area, path, &th, focused, None);

    let wants_chrome = area.height >= MIN_CHROME_HEIGHT;
    if wants_chrome && y < max_y {
        f.render_widget(
            Line::from(Span::styled(
                meta_line(app, info),
                Style::default().fg(th.fg_secondary),
            )),
            Rect::new(area.x, y, area.width, 1),
        );
        y += 1;
        if y < max_y {
            y += 1;
        }
    }

    // Reserve the transport row before handing the rest to the frame, so the
    // frame is never sized into space the status line then draws over.
    let footer_y = (wants_chrome && max_y > y).then(|| max_y - 1);
    let frame_bottom = footer_y.unwrap_or(max_y);
    if y >= frame_bottom {
        return;
    }
    let frame_area = Rect::new(area.x, y, area.width, frame_bottom - y);
    render_frame(f, app, frame_area);

    if let Some(footer_y) = footer_y {
        render_transport(f, app, Rect::new(area.x, footer_y, area.width, 1));
    }
}

/// Draw the current frame centred in `area`, or the reason there isn't one.
fn render_frame(f: &mut Frame, app: &mut App, area: Rect) {
    if area.width < 1 || area.height < 1 {
        return;
    }

    // Geometry is terminal-local cached layout. `tick` compares it with the
    // current player and schedules any decoder rebuild off the render path.
    app.last_video_frame_area = Some(area);

    let placed = app.video.as_ref().and_then(|player| {
        player
            .frame()
            .map(|(frame, frame_area)| (frame, centered(area, frame_area)))
    });

    match placed {
        Some((frame, frame_area)) => f.render_widget(Image::new(frame), frame_area),
        None => render_centered_note(f, app, area, &pending_note(app)),
    }
}

/// The transport line: position over duration, a progress rule, and the key
/// hint for whatever pressing `p` would do next.
fn render_transport(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;
    let Some(player) = app.video.as_ref() else {
        return;
    };
    let info = player.info();

    let position = player.position();
    let clock = match info.duration {
        Some(duration) => format!(
            "{} / {}",
            format_timecode(position),
            format_timecode(duration)
        ),
        None => format_timecode(position),
    };
    let hint = t(if player.has_ended() {
        Msg::PreviewVideoReplayHint
    } else if player.is_playing() {
        Msg::PreviewVideoPauseHint
    } else {
        Msg::PreviewVideoPlayHint
    });

    let clock_w = UnicodeWidthStr::width(clock.as_str()) as u16;
    let hint_w = UnicodeWidthStr::width(hint) as u16;
    // Two single-space gaps around the rule; without room for all three
    // pieces the rule is dropped rather than squeezed to nothing.
    let rule_w = area
        .width
        .saturating_sub(clock_w)
        .saturating_sub(hint_w)
        .saturating_sub(4);

    let mut spans = vec![Span::styled(
        clock,
        Style::default()
            .fg(th.fg_secondary)
            .add_modifier(Modifier::BOLD),
    )];
    if rule_w > 0 {
        let filled = progress_cells(position, info.duration, rule_w);
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            "━".repeat(filled as usize),
            Style::default().fg(th.accent),
        ));
        spans.push(Span::styled(
            "─".repeat((rule_w - filled) as usize),
            Style::default().fg(th.fg_secondary),
        ));
        spans.push(Span::raw("  "));
    } else {
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(hint, Style::default().fg(th.fg_secondary)));

    f.render_widget(Line::from(spans), area);
}

/// How many of `width` cells the progress rule should fill. A source without
/// a declared duration has no meaningful progress, so the rule stays empty.
fn progress_cells(position: f64, duration: Option<f64>, width: u16) -> u16 {
    let Some(duration) = duration.filter(|d| *d > 0.0) else {
        return 0;
    };
    let ratio = (position / duration).clamp(0.0, 1.0);
    (ratio * width as f64).round() as u16
}

/// Centre `frame` inside `area`, keeping it fully inside the bounds.
fn centered(area: Rect, frame: Rect) -> Rect {
    let width = frame.width.min(area.width);
    let height = frame.height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

/// `1920×1080 · 12s · 4.2 MB` — dimensions come from the probe once it has
/// run, so before that the line is the plain binary metadata.
fn meta_line(app: &App, info: &BinaryInfo) -> String {
    let Some(player) = app.video.as_ref() else {
        return info.meta_line.clone();
    };
    let probed = player.info();
    let mut parts = vec![format!("{}×{}", probed.width, probed.height)];
    if let Some(duration) = probed.duration {
        parts.push(format_timecode(duration));
    }
    if !info.meta_line.is_empty() {
        parts.push(info.meta_line.clone());
    }
    parts.join(" · ")
}

/// What to say in place of a frame: either why inline playback is off, or
/// that the clip is still opening.
fn pending_note(app: &App) -> String {
    match app.video_status.as_ref() {
        Some(VideoUnavailable::NoFfmpeg) => t(Msg::PreviewVideoNeedsFfmpeg).to_string(),
        Some(VideoUnavailable::Tmux) => t(Msg::PreviewVideoTmux).to_string(),
        Some(VideoUnavailable::Remote) => t(Msg::PreviewVideoRemote).to_string(),
        Some(VideoUnavailable::Unreadable(detail)) => {
            format!("{}: {detail}", t(Msg::PreviewVideoUnreadable))
        }
        // An explicit `REEF_VIDEO=off` and a terminal that simply can't do it
        // read the same way from the card: this is a still, and that's it.
        Some(VideoUnavailable::Disabled | VideoUnavailable::UnsupportedTerminal) => {
            t(Msg::PreviewVideoUnsupportedTerminal).to_string()
        }
        None => t(Msg::PreviewLoading).to_string(),
    }
}

fn render_centered_note(f: &mut Frame, app: &App, area: Rect, note: &str) {
    let width = UnicodeWidthStr::width(note) as u16;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height / 2;
    f.render_widget(
        Line::from(Span::styled(
            note,
            Style::default().fg(app.theme.fg_secondary),
        )),
        Rect::new(x, y, area.width.saturating_sub(x - area.x), 1),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_fills_proportionally() {
        assert_eq!(progress_cells(0.0, Some(10.0), 20), 0);
        assert_eq!(progress_cells(5.0, Some(10.0), 20), 10);
        assert_eq!(progress_cells(10.0, Some(10.0), 20), 20);
    }

    #[test]
    fn progress_is_empty_without_a_duration() {
        assert_eq!(progress_cells(5.0, None, 20), 0);
        assert_eq!(progress_cells(5.0, Some(0.0), 20), 0);
    }

    #[test]
    fn progress_clamps_past_the_end() {
        assert_eq!(progress_cells(99.0, Some(10.0), 20), 20);
    }

    #[test]
    fn frame_is_centered_and_clamped() {
        let area = Rect::new(10, 5, 40, 20);
        let placed = centered(area, Rect::new(0, 0, 20, 10));
        assert_eq!(placed, Rect::new(20, 10, 20, 10));

        // An oversized frame is clipped to the area rather than overflowing.
        let clamped = centered(area, Rect::new(0, 0, 100, 100));
        assert_eq!(clamped, Rect::new(10, 5, 40, 20));
    }
}
