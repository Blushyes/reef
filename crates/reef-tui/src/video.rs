//! Inline video playback for the preview panel.
//!
//! Terminals cannot decode video, only display still images. So playback is
//! ffmpeg decoding straight into pre-sized RGBA frames, which we hand to the
//! same terminal graphics protocols the image preview already uses (Kitty
//! unicode-placeholders / iTerm2 inline images). One frame replaces the
//! previous one on every tick — that is the whole trick.
//!
//! The pipeline is deliberately transcode-free on our side:
//!
//! ```text
//!   ffprobe  →  dimensions / fps / duration
//!   ffmpeg   →  fps + scale + pad  →  rawvideo rgba on stdout
//!   reader thread  →  read_exact(frame_bytes)  →  bounded channel
//!   tick()   →  wrap bytes as RgbaImage  →  protocol encode  →  buffer
//! ```
//!
//! Frames leave ffmpeg already scaled to the exact pixel size the panel can
//! show, so no resizing, no image decoding, and no re-encoding happens per
//! frame. The bounded channel is the flow control: a paused player simply
//! stops draining it, ffmpeg blocks on a full pipe, and no work is done until
//! playback resumes.
//!
//! `REEF_VIDEO` is an escape hatch:
//!   - `off` / `none` — never play inline; the card stays a still card.
//!   - anything else / unset — play when the terminal and source allow it.
//!
//! `REEF_VIDEO_FPS` overrides the playback frame rate (1–60).

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

use image::{DynamicImage, RgbaImage};
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::{Protocol, iterm2::Iterm2, kitty::Kitty};

/// Playback frame rate ceiling. Terminal graphics are re-transmitted whole on
/// every frame, so the cost scales with frame rate; 15 fps reads as motion
/// while leaving the render loop (a 16 ms poll) most of its budget.
const DEFAULT_MAX_FPS: f64 = 15.0;

/// Kitty image id reserved for video frames. Every frame re-transmits under
/// this same id, which makes the terminal replace the previous frame's data
/// instead of accumulating one image per frame in its cache.
const KITTY_VIDEO_ID: u32 = 0x5245_4600;

/// Decoded frames buffered ahead of playback. Deep enough to keep ffmpeg
/// and the render loop working at the same time, shallow enough that a
/// paused clip stops decoding almost immediately.
const FRAME_QUEUE_DEPTH: usize = 3;

/// How long `open` waits for ffmpeg to hand over the first frame before
/// giving up. Generous because it covers process spawn plus the decode of
/// however much container the first keyframe sits behind.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a video is showing as a still card instead of playing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoUnavailable {
    /// `REEF_VIDEO=off`.
    Disabled,
    /// The terminal has no graphics protocol we can push frames through.
    UnsupportedTerminal,
    /// Inside tmux, where inline graphics need passthrough we don't drive.
    Tmux,
    /// The file lives on the remote side of an SSH session — there is no
    /// host-local path for ffmpeg to read.
    Remote,
    /// ffmpeg / ffprobe are not on `PATH`.
    NoFfmpeg,
    /// ffprobe ran but the file yielded no usable video stream.
    Unreadable(String),
}

/// What ffprobe reports about the source file.
#[derive(Debug, Clone, Copy)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    /// Source frame rate; `None` when the container doesn't declare one.
    pub fps: Option<f64>,
    /// Duration in seconds; `None` for streams without one.
    pub duration: Option<f64>,
}

/// Playback state machine for one preview panel.
pub struct VideoPlayer {
    path: PathBuf,
    /// Renderer-neutral preview revision that produced this player. Binary
    /// previews advance it on every accepted reload, including same-size
    /// rewrites that cannot be identified from file metadata alone.
    source_revision: u64,
    info: VideoInfo,
    /// Playback frame rate — the source rate clamped to `DEFAULT_MAX_FPS`.
    fps: f64,
    /// Cell area the current decoder was sized for. A panel resize makes this
    /// stale and restarts ffmpeg at the new size.
    cell_area: Rect,
    /// Pixel dimensions of every frame ffmpeg emits.
    frame_px: (u32, u32),
    decoder: Option<Decoder>,
    /// The most recently encoded frame, ready for the widget to render.
    frame: Option<Protocol>,
    /// Frames consumed since the decoder started, plus wherever the decoder
    /// was seeked to. Drives the progress readout.
    position: f64,
    playing: bool,
    ended: bool,
    /// When the next frame is due. `None` while paused.
    next_due: Option<Instant>,
}

/// Cheap, cloneable source state used to rebuild a player off the UI thread.
#[derive(Clone)]
pub(crate) struct VideoSource {
    path: PathBuf,
    source_revision: u64,
    info: VideoInfo,
    fps: f64,
}

/// A running ffmpeg process and the thread draining its stdout.
struct Decoder {
    child: Option<Child>,
    frames: Receiver<Vec<u8>>,
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.kill();
        // Waiting can block while ffmpeg handles termination. Reap it away
        // from the UI thread; closing the receiver after this method returns
        // also releases the stdout reader if it was waiting on channel space.
        let _ = std::thread::Builder::new()
            .name("reef-video-reap".into())
            .spawn(move || {
                let _ = child.wait();
            });
    }
}

impl VideoPlayer {
    /// Probe `path`, start ffmpeg sized for `cell_area`, and block until the
    /// first frame is on screen. The player comes back paused: opening a
    /// video shows its opening frame, and playback is an explicit key away.
    pub fn open(
        path: &Path,
        source_revision: u64,
        picker: &Picker,
        cell_area: Rect,
    ) -> Result<VideoPlayer, VideoUnavailable> {
        if disabled_by_env() {
            return Err(VideoUnavailable::Disabled);
        }
        if in_tmux() {
            return Err(VideoUnavailable::Tmux);
        }
        if !protocol_can_animate(picker.protocol_type()) {
            return Err(VideoUnavailable::UnsupportedTerminal);
        }

        let info = probe(path)?;
        let fps = playback_fps(info.fps);
        let source = VideoSource {
            path: path.to_path_buf(),
            source_revision,
            info,
            fps,
        };
        Self::open_source(source, picker, cell_area, 0.0)
    }

    pub(crate) fn open_source(
        source: VideoSource,
        picker: &Picker,
        cell_area: Rect,
        position: f64,
    ) -> Result<VideoPlayer, VideoUnavailable> {
        let VideoSource {
            path,
            source_revision,
            info,
            fps,
        } = source;
        let (frame_px, area) = frame_size(info, picker, cell_area);

        let mut player = VideoPlayer {
            path,
            source_revision,
            info,
            fps,
            cell_area: area,
            frame_px,
            decoder: None,
            frame: None,
            position,
            playing: false,
            ended: false,
            next_due: None,
        };
        player.start_decoder(position)?;
        // Pull the opening frame synchronously so the card never flashes an
        // empty box between selection and first paint.
        player.await_first_frame(picker)?;
        Ok(player)
    }

    pub fn info(&self) -> VideoInfo {
        self.info
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this player is still showing the file the preview describes.
    /// A rewritten clip needs a new player, not a resumed one.
    pub fn matches_source(&self, path: &Path, source_revision: u64) -> bool {
        self.path == path && self.source_revision == source_revision
    }

    pub(crate) fn source(&self) -> VideoSource {
        VideoSource {
            path: self.path.clone(),
            source_revision: self.source_revision,
            info: self.info,
            fps: self.fps,
        }
    }

    pub(crate) fn source_revision(&self) -> u64 {
        self.source_revision
    }

    /// Build a replacement player at `position` and `cell_area`.
    ///
    /// This probes no source metadata, but it starts ffmpeg and waits for its
    /// first frame. Callers must run it on a worker thread.
    pub fn rebuild(
        &self,
        picker: &Picker,
        cell_area: Rect,
        position: f64,
    ) -> Result<VideoPlayer, VideoUnavailable> {
        Self::open_source(self.source(), picker, cell_area, position)
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn has_ended(&self) -> bool {
        self.ended
    }

    /// Seconds played so far, clamped to the source duration.
    pub fn position(&self) -> f64 {
        match self.info.duration {
            Some(duration) => self.position.min(duration),
            None => self.position,
        }
    }

    /// The frame to draw, and the cell area it occupies.
    pub fn frame(&self) -> Option<(&Protocol, Rect)> {
        self.frame.as_ref().map(|frame| (frame, self.cell_area))
    }

    pub(crate) fn set_playing(&mut self, playing: bool) {
        self.playing = playing && !self.ended;
        self.next_due = self.playing.then(|| Instant::now() + self.frame_interval());
    }

    /// Toggle play/pause without performing I/O. Returns `false` when the
    /// clip has ended and must be rebuilt asynchronously from the beginning.
    pub fn toggle(&mut self) -> bool {
        if self.ended {
            return false;
        }
        self.set_playing(!self.playing);
        true
    }

    pub fn pause(&mut self) {
        self.playing = false;
        self.next_due = None;
    }

    pub fn matches_area(&self, picker: &Picker, cell_area: Rect) -> bool {
        let (frame_px, area) = frame_size(self.info, picker, cell_area);
        frame_px == self.frame_px && area == self.cell_area
    }

    /// Advance playback. Returns whether the visible frame changed, which the
    /// caller turns into a redraw.
    pub fn tick(&mut self, now: Instant, picker: &Picker) -> bool {
        if !self.playing || self.ended {
            return false;
        }
        let Some(due) = self.next_due else {
            return false;
        };
        if now < due {
            return false;
        }

        match self.take_frame() {
            FramePull::Frame(bytes) => {
                self.encode(picker, bytes);
                self.position += 1.0 / self.fps;
                // Schedule from the deadline, not from `now`, so playback
                // keeps the source's timing instead of drifting by however
                // late this tick ran. A tick that fell far behind (panel
                // busy, terminal stalled) resyncs to now rather than trying
                // to catch up through a burst of frames.
                let next = due + self.frame_interval();
                self.next_due = Some(if next < now {
                    now + self.frame_interval()
                } else {
                    next
                });
                true
            }
            // Decoder hasn't produced the next frame yet — hold the current
            // one and look again shortly.
            FramePull::Pending => {
                self.next_due = Some(now + self.frame_interval());
                false
            }
            FramePull::Finished => self.finish(),
        }
    }

    /// Settle into the ended state: no decoder, no schedule, position parked
    /// at the duration. The last encoded frame stays on screen. Always
    /// reports `true` so the card redraws with its replay hint.
    fn finish(&mut self) -> bool {
        self.ended = true;
        self.playing = false;
        self.next_due = None;
        self.decoder = None;
        if let Some(duration) = self.info.duration {
            self.position = duration;
        }
        true
    }

    fn frame_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps)
    }

    /// Spawn ffmpeg decoding from `start_secs` at the current frame size.
    /// Replaces any decoder already running.
    fn start_decoder(&mut self, start_secs: f64) -> Result<(), VideoUnavailable> {
        // Dropping first kills the previous ffmpeg, so two decoders never
        // compete for the terminal's frame budget.
        self.decoder = None;

        let (width, height) = self.frame_px;
        let mut command = Command::new("ffmpeg");
        command.arg("-hide_banner").args(["-loglevel", "error"]);
        if start_secs > 0.0 {
            // Input seeking: ffmpeg jumps by keyframe before decoding, which
            // is what makes a resize mid-playback cheap.
            command.args(["-ss", &format!("{start_secs:.3}")]);
        }
        command
            .arg("-i")
            .arg(&self.path)
            // No audio path exists in a terminal, so never decode it.
            .arg("-an")
            .args(["-vf", &filter_chain(width, height, self.fps)])
            .args(["-f", "rawvideo"])
            .args(["-pix_fmt", "rgba"])
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = command.spawn().map_err(|_| VideoUnavailable::NoFfmpeg)?;
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            return Err(VideoUnavailable::Unreadable("ffmpeg stdout".into()));
        };

        // A small buffer of decoded frames is what lets decoding overlap
        // with encoding and rendering — at depth 1 the two sides hand off
        // in lockstep and each waits on the other, roughly halving the
        // frame rate a given clip can sustain. It stays small because it is
        // also the flow control: pausing stops draining, the channel fills
        // within a few frames, ffmpeg blocks on a full pipe, and a paused
        // video settles back to costing nothing.
        let (tx, rx) = sync_channel::<Vec<u8>>(FRAME_QUEUE_DEPTH);
        let frame_bytes = width as usize * height as usize * 4;
        std::thread::Builder::new()
            .name("reef-video-decode".into())
            .spawn(move || read_frames(stdout, frame_bytes, tx))
            .map_err(|e| VideoUnavailable::Unreadable(e.to_string()))?;

        self.decoder = Some(Decoder {
            child: Some(child),
            frames: rx,
        });
        Ok(())
    }

    /// Block briefly for the decoder's first frame so a newly opened or
    /// resized player has something to show immediately.
    fn await_first_frame(&mut self, picker: &Picker) -> Result<(), VideoUnavailable> {
        let Some(decoder) = self.decoder.as_ref() else {
            return Err(VideoUnavailable::Unreadable("decoder unavailable".into()));
        };
        let bytes = first_frame_bytes(decoder.frames.recv_timeout(FIRST_FRAME_TIMEOUT))?;
        self.encode(picker, bytes);
        if self.frame.is_some() {
            Ok(())
        } else {
            Err(VideoUnavailable::Unreadable(
                "could not encode first frame".into(),
            ))
        }
    }

    fn take_frame(&mut self) -> FramePull {
        let Some(decoder) = self.decoder.as_ref() else {
            return FramePull::Finished;
        };
        match decoder.frames.try_recv() {
            Ok(bytes) => FramePull::Frame(bytes),
            Err(TryRecvError::Empty) => FramePull::Pending,
            Err(TryRecvError::Disconnected) => FramePull::Finished,
        }
    }

    /// Wrap raw RGBA bytes as an image and encode them for the terminal.
    /// Nothing is resized or re-compressed here — the buffer is already the
    /// exact frame the panel shows.
    fn encode(&mut self, picker: &Picker, bytes: Vec<u8>) {
        let (width, height) = self.frame_px;
        let Some(buffer) = RgbaImage::from_raw(width, height, bytes) else {
            return;
        };
        let image = DynamicImage::ImageRgba8(buffer);
        let encoded = match picker.protocol_type() {
            ProtocolType::Kitty => Kitty::new(image, self.cell_area, KITTY_VIDEO_ID, false)
                .ok()
                .map(Protocol::Kitty),
            ProtocolType::Iterm2 => Iterm2::new(image, self.cell_area, false)
                .ok()
                .map(Protocol::ITerm2),
            // `open` refuses these protocols, so this is unreachable in
            // practice; dropping the frame is the safe reading either way.
            ProtocolType::Halfblocks | ProtocolType::Sixel => None,
        };
        if let Some(encoded) = encoded {
            self.frame = Some(encoded);
        }
    }
}

fn first_frame_bytes(
    result: Result<Vec<u8>, RecvTimeoutError>,
) -> Result<Vec<u8>, VideoUnavailable> {
    result.map_err(|error| match error {
        RecvTimeoutError::Timeout => {
            VideoUnavailable::Unreadable("timed out waiting for first frame".into())
        }
        RecvTimeoutError::Disconnected => {
            VideoUnavailable::Unreadable("ffmpeg produced no frames".into())
        }
    })
}

enum FramePull {
    Frame(Vec<u8>),
    Pending,
    Finished,
}

/// Drain fixed-size frames off ffmpeg's stdout until the stream ends or the
/// receiver goes away.
fn read_frames(stdout: std::process::ChildStdout, frame_bytes: usize, tx: SyncSender<Vec<u8>>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut frame = vec![0u8; frame_bytes];
        if reader.read_exact(&mut frame).is_err() {
            // End of stream, or a truncated final frame — both mean done.
            return;
        }
        // Send failure means the player was dropped or a resize replaced this
        // decoder; either way this thread's frames are no longer wanted.
        if tx.send(frame).is_err() {
            return;
        }
    }
}

/// The ffmpeg filter chain: drop to playback frame rate, fit inside the
/// frame, then pad back out to the exact size so every frame is the same
/// number of bytes. Padding is transparent, so the letterboxed margins show
/// the terminal background rather than black bars.
fn filter_chain(width: u32, height: u32, fps: f64) -> String {
    format!(
        "fps={fps:.4},scale={width}:{height}:force_original_aspect_ratio=decrease\
         :flags=fast_bilinear,\
         format=rgba,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=0x00000000"
    )
}

/// Pick the frame pixel size and the cell area it maps to. The frame is sized
/// to whole cells so the terminal never has to stretch it to fit the grid.
fn frame_size(info: VideoInfo, picker: &Picker, cell_area: Rect) -> ((u32, u32), Rect) {
    let (font_w, font_h) = picker.font_size();
    let font_w = font_w.max(1) as u32;
    let font_h = font_h.max(1) as u32;

    let max_cols = cell_area.width.max(1) as u32;
    let max_rows = cell_area.height.max(1) as u32;
    let max_px_w = max_cols * font_w;
    let max_px_h = max_rows * font_h;

    // Fit the source inside the panel without upscaling past its own
    // resolution — a 320×240 clip in a huge panel stays sharp instead of
    // being blown up by ffmpeg.
    let src_w = info.width.max(1);
    let src_h = info.height.max(1);
    let scale = (max_px_w as f64 / src_w as f64)
        .min(max_px_h as f64 / src_h as f64)
        .min(1.0);
    let fit_w = ((src_w as f64 * scale).round() as u32).max(1);
    let fit_h = ((src_h as f64 * scale).round() as u32).max(1);

    // Round up to whole cells, then clamp back inside the panel.
    let cols = fit_w.div_ceil(font_w).clamp(1, max_cols);
    let rows = fit_h.div_ceil(font_h).clamp(1, max_rows);

    let area = Rect::new(cell_area.x, cell_area.y, cols as u16, rows as u16);
    ((cols * font_w, rows * font_h), area)
}

fn playback_fps(source_fps: Option<f64>) -> f64 {
    let ceiling = std::env::var("REEF_VIDEO_FPS")
        .ok()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|fps| fps.is_finite())
        .map(|fps| fps.clamp(1.0, 60.0))
        .unwrap_or(DEFAULT_MAX_FPS);
    match source_fps {
        Some(fps) if fps.is_finite() && fps > 0.0 => fps.min(ceiling),
        _ => ceiling,
    }
}

fn disabled_by_env() -> bool {
    std::env::var("REEF_VIDEO")
        .map(|raw| matches!(raw.trim().to_ascii_lowercase().as_str(), "off" | "none"))
        .unwrap_or(false)
}

fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
}

/// Only the protocols that can replace an image in place are worth animating.
/// Sixel re-encodes the whole frame in-band on the render thread, and
/// halfblocks would repaint every cell — both cost more per frame than the
/// result is worth, so those terminals keep the still card.
fn protocol_can_animate(protocol: ProtocolType) -> bool {
    matches!(protocol, ProtocolType::Kitty | ProtocolType::Iterm2)
}

/// Ask ffprobe for the source's dimensions, frame rate, and duration.
fn probe(path: &Path) -> Result<VideoInfo, VideoUnavailable> {
    let output = Command::new("ffprobe")
        .args(["-v", "error"])
        .args(["-select_streams", "v:0"])
        .args(["-show_entries", "stream=width,height,r_frame_rate"])
        .args(["-show_entries", "format=duration"])
        .args(["-of", "default=noprint_wrappers=1"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|_| VideoUnavailable::NoFfmpeg)?;

    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(VideoUnavailable::Unreadable(first_line(&detail)));
    }
    parse_probe(&String::from_utf8_lossy(&output.stdout))
}

/// Parse ffprobe's `key=value` lines. Width and height are required — a file
/// without them has no video stream we can show.
fn parse_probe(text: &str) -> Result<VideoInfo, VideoUnavailable> {
    let mut width = None;
    let mut height = None;
    let mut fps = None;
    let mut duration = None;

    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "width" => width = value.parse::<u32>().ok(),
            "height" => height = value.parse::<u32>().ok(),
            "r_frame_rate" => fps = parse_rational(value),
            "duration" => duration = value.parse::<f64>().ok().filter(|d| d.is_finite()),
            _ => {}
        }
    }

    match (width, height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => Ok(VideoInfo {
            width,
            height,
            fps,
            duration,
        }),
        _ => Err(VideoUnavailable::Unreadable("no video stream".into())),
    }
}

/// ffprobe reports frame rates as exact rationals (`30000/1001`).
fn parse_rational(value: &str) -> Option<f64> {
    let (num, den) = value.split_once('/')?;
    let num: f64 = num.trim().parse().ok()?;
    let den: f64 = den.trim().parse().ok()?;
    (den != 0.0 && num > 0.0).then(|| num / den)
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("ffprobe failed")
        .to_string()
}

/// Rows the video card spends on chrome: the two-line header, the metadata
/// line and the blank under it, and the footer status line.
const CARD_CHROME_ROWS: u16 = 5;

/// The frame area inside a preview panel of `rect`, used to size a decoder
/// before the card has ever been rendered. Falls back to a small but usable
/// box when no panel rect is known yet; the adapter schedules a rebuild after
/// the first rendered layout is cached.
pub fn preview_frame_area(rect: Option<Rect>) -> Rect {
    const FALLBACK: Rect = Rect {
        x: 0,
        y: 0,
        width: 60,
        height: 20,
    };
    let Some(rect) = rect else {
        return FALLBACK;
    };
    Rect::new(
        rect.x,
        rect.y.saturating_add(CARD_CHROME_ROWS - 1),
        rect.width,
        rect.height.saturating_sub(CARD_CHROME_ROWS).max(1),
    )
}

/// `m:ss` for a seconds count, for the progress readout.
pub fn format_timecode(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picker with a known cell size, so the geometry assertions below
    /// don't depend on a real terminal. `from_fontsize` is the only way to
    /// state a cell size directly; the suggested replacements either query
    /// a terminal or hardcode the halfblocks size.
    #[allow(deprecated)]
    fn picker_with_cells(font_size: (u16, u16)) -> Picker {
        Picker::from_fontsize(font_size)
    }

    fn info(width: u32, height: u32) -> VideoInfo {
        VideoInfo {
            width,
            height,
            fps: Some(30.0),
            duration: Some(10.0),
        }
    }

    #[test]
    fn parses_probe_output() {
        let parsed =
            parse_probe("width=1920\nheight=1080\nr_frame_rate=30000/1001\nduration=12.5\n")
                .expect("probe should parse");
        assert_eq!((parsed.width, parsed.height), (1920, 1080));
        assert!((parsed.fps.unwrap() - 29.97).abs() < 0.01);
        assert_eq!(parsed.duration, Some(12.5));
    }

    #[test]
    fn probe_without_dimensions_is_unreadable() {
        assert!(parse_probe("duration=12.5\n").is_err());
    }

    #[test]
    fn probe_tolerates_missing_optional_fields() {
        let parsed = parse_probe("width=640\nheight=480\nr_frame_rate=0/0\nduration=N/A\n")
            .expect("dimensions alone are enough");
        assert_eq!(parsed.fps, None);
        assert_eq!(parsed.duration, None);
    }

    #[test]
    fn playback_fps_is_capped_but_never_exceeds_source() {
        assert_eq!(playback_fps(Some(60.0)), DEFAULT_MAX_FPS);
        assert_eq!(playback_fps(Some(8.0)), 8.0);
        assert_eq!(playback_fps(None), DEFAULT_MAX_FPS);
    }

    #[test]
    fn frame_size_lands_on_whole_cells_within_the_panel() {
        let picker = picker_with_cells((10, 20));
        let area = Rect::new(3, 4, 40, 20);
        let ((px_w, px_h), cells) = frame_size(info(1920, 1080), &picker, area);

        assert_eq!(px_w % 10, 0);
        assert_eq!(px_h % 20, 0);
        assert!(cells.width <= area.width && cells.height <= area.height);
        assert_eq!((cells.x, cells.y), (area.x, area.y));
        assert_eq!(
            (px_w, px_h),
            (cells.width as u32 * 10, cells.height as u32 * 20)
        );
    }

    #[test]
    fn frame_size_does_not_upscale_small_sources() {
        let picker = picker_with_cells((10, 20));
        // A 100×80 clip inside a panel with room for 400×400 px keeps its own
        // resolution, rounded up to the enclosing cells.
        let ((px_w, px_h), _) = frame_size(info(100, 80), &picker, Rect::new(0, 0, 40, 20));
        assert_eq!((px_w, px_h), (100, 80));
    }

    #[test]
    fn frame_size_survives_a_degenerate_panel() {
        let picker = picker_with_cells((10, 20));
        let ((px_w, px_h), cells) = frame_size(info(1920, 1080), &picker, Rect::new(0, 0, 0, 0));
        assert!(px_w > 0 && px_h > 0);
        assert_eq!((cells.width, cells.height), (1, 1));
    }

    #[test]
    fn filter_chain_pads_to_a_fixed_frame_size() {
        let chain = filter_chain(320, 240, 15.0);
        assert!(chain.contains("fps=15.0000"));
        assert!(
            chain
                .contains("scale=320:240:force_original_aspect_ratio=decrease:flags=fast_bilinear")
        );
        assert!(chain.contains("pad=320:240"));
        assert!(chain.contains("color=0x00000000"));
    }

    #[test]
    fn timecode_formats_minutes_and_seconds() {
        assert_eq!(format_timecode(0.0), "0:00");
        assert_eq!(format_timecode(9.6), "0:10");
        assert_eq!(format_timecode(125.0), "2:05");
        assert_eq!(format_timecode(-3.0), "0:00");
    }

    #[test]
    fn only_replaceable_protocols_animate() {
        assert!(protocol_can_animate(ProtocolType::Kitty));
        assert!(protocol_can_animate(ProtocolType::Iterm2));
        assert!(!protocol_can_animate(ProtocolType::Sixel));
        assert!(!protocol_can_animate(ProtocolType::Halfblocks));
    }

    #[test]
    fn disconnected_first_frame_is_unreadable() {
        assert_eq!(
            first_frame_bytes(Err(RecvTimeoutError::Disconnected)),
            Err(VideoUnavailable::Unreadable(
                "ffmpeg produced no frames".into()
            ))
        );
    }

    #[test]
    fn timed_out_first_frame_is_unreadable() {
        assert_eq!(
            first_frame_bytes(Err(RecvTimeoutError::Timeout)),
            Err(VideoUnavailable::Unreadable(
                "timed out waiting for first frame".into()
            ))
        );
    }
}
