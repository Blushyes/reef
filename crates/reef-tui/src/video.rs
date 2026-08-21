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
//!   ffprobe  →  dimensions / fps / duration / audio presence
//!   ffmpeg   →  fps + display-correct scale  →  rawvideo rgba on stdout
//!   ffplay   →  audio device + audio-master clock on stderr
//!   reader threads  →  frame queue + clock IPC
//!   tick()   →  drop late frames, queue the newest due frame
//!   encoder thread  →  terminal protocol  →  buffer
//! ```
//!
//! Frames leave ffmpeg already scaled to the exact pixel size the panel can
//! show, so no resizing or image decoding happens per frame. For clips with
//! audio, ffplay's device clock is authoritative: frames behind it are
//! drained without encoding and only the newest due frame reaches the
//! terminal. Pausing stops ffplay and leaves the bounded video queue full, so
//! neither decoder consumes resources until playback resumes.
//!
//! `REEF_VIDEO` is an escape hatch:
//!   - `off` / `none` — never play inline; the card stays a still card.
//!   - anything else / unset — play when the terminal and source allow it.
//!
//! `REEF_VIDEO_FPS` overrides the playback frame rate (1–60).

mod audio;

use self::audio::{AudioPlayer, AudioPollState};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::time::{Duration, Instant};

use image::{DynamicImage, RgbaImage};
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::{Protocol, iterm2::Iterm2, kitty::Kitty};

/// Playback frame rate ceiling. Terminal graphics are re-transmitted whole on
/// every frame, so the cost scales with frame rate; 15 fps reads as motion
/// while leaving the render loop (a 16 ms poll) most of its budget.
const DEFAULT_MAX_FPS: f64 = 15.0;

/// Maximum protocol payload we budget for one uncompressed RGBA frame.
/// The estimator below reserves six bytes per pixel for RGBA base64 plus
/// chunk commands, so three MiB corresponds to 524,288 pixels (roughly
/// 966×543 at 16:9). Keeping the bound in wire bytes makes the reason for the
/// limit explicit: terminal output, not ffmpeg decode, is the bottleneck.
const MAX_FRAME_WIRE_BYTES: u64 = 3 * 1024 * 1024;

const ESTIMATED_WIRE_BYTES_PER_PIXEL: u64 = 6;

/// Aggregate terminal bandwidth reserved for video frames. The frame-size
/// cap above keeps a single draw bounded; this cap lowers FPS as frames grow
/// so repeated draws stay responsive too.
const MAX_VIDEO_WIRE_BYTES_PER_SECOND: u64 = 32 * 1024 * 1024;

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
    /// The source has audio, but ffplay is not on `PATH`.
    NoFfplay,
    /// ffprobe ran but the file yielded no usable video stream.
    Unreadable(String),
    /// A newer preview, seek, or resize superseded this open request.
    Cancelled,
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
    /// Whether the container has an audio stream that can be the playback
    /// clock.
    pub has_audio: bool,
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
    encoder: Option<FrameEncoder>,
    /// The most recently encoded frame, ready for the widget to render.
    frame: Option<Protocol>,
    /// Master playback position. Driven by the audio clock when available,
    /// otherwise by `wall_clock`.
    position: f64,
    /// Timestamp of the frame currently encoded for the terminal. This may
    /// trail `position` by at most one output frame.
    frame_position: f64,
    playing: bool,
    ended: bool,
    /// Wall clock used when no audio process is available to supply one.
    wall_clock: Option<PlaybackClock>,
    audio: Option<AudioPlayer>,
    playback_error: Option<VideoUnavailable>,
}

/// Cheap, cloneable source state used to rebuild a player off the UI thread.
#[derive(Clone)]
pub(crate) struct VideoSource {
    path: PathBuf,
    source_revision: u64,
    info: VideoInfo,
}

/// A running ffmpeg process and the thread draining its stdout.
struct Decoder {
    child: Option<Child>,
    frames: Receiver<Vec<u8>>,
}

/// One in-flight terminal encoding plus the newest frame that arrived while
/// it was busy. Replacing `pending` bounds memory and prevents stale frames
/// from building up behind a slow terminal protocol encoder.
struct FrameEncoder {
    requests: SyncSender<Vec<u8>>,
    results: Receiver<Option<Protocol>>,
    busy: bool,
    pending: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy)]
struct PlaybackClock {
    position: f64,
    started_at: Instant,
}

#[derive(Debug, Default, Clone, Copy)]
struct AdvanceResult {
    decoder_finished: bool,
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        kill_and_reap(child, "reef-video-reap");
    }
}

impl FrameEncoder {
    fn spawn(
        picker: Picker,
        frame_px: (u32, u32),
        cell_area: Rect,
    ) -> Result<Self, VideoUnavailable> {
        let (request_tx, request_rx) = sync_channel(1);
        let (result_tx, result_rx) = sync_channel(1);
        std::thread::Builder::new()
            .name("reef-video-encode".into())
            .spawn(move || {
                while let Ok(bytes) = request_rx.recv() {
                    let encoded = encode_frame(&picker, frame_px, cell_area, bytes);
                    if result_tx.send(encoded).is_err() {
                        return;
                    }
                }
            })
            .map_err(|error| VideoUnavailable::Unreadable(error.to_string()))?;
        Ok(Self {
            requests: request_tx,
            results: result_rx,
            busy: false,
            pending: None,
        })
    }

    fn submit(&mut self, bytes: Vec<u8>) {
        if self.busy {
            self.pending = Some(bytes);
            return;
        }
        match self.requests.try_send(bytes) {
            Ok(()) => self.busy = true,
            Err(TrySendError::Full(bytes)) => {
                self.busy = true;
                self.pending = Some(bytes);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn poll(&mut self) -> Option<Protocol> {
        let result = match self.results.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return None,
        };
        self.busy = false;
        if let Some(bytes) = self.pending.take() {
            self.submit(bytes);
        }
        result
    }
}

impl PlaybackClock {
    fn new(position: f64, started_at: Instant) -> Self {
        Self {
            position,
            started_at,
        }
    }

    fn position_at(self, now: Instant) -> f64 {
        self.position + now.saturating_duration_since(self.started_at).as_secs_f64()
    }
}

fn kill_and_reap(mut child: Child, thread_name: &'static str) {
    let _ = child.kill();
    let _ = std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            let _ = child.wait();
        });
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
        Self::open_cancellable(
            path,
            source_revision,
            picker,
            cell_area,
            &reef_io::CancellationToken::default(),
        )
    }

    pub(crate) fn open_cancellable(
        path: &Path,
        source_revision: u64,
        picker: &Picker,
        cell_area: Rect,
        cancellation: &reef_io::CancellationToken,
    ) -> Result<VideoPlayer, VideoUnavailable> {
        ensure_not_cancelled(cancellation)?;
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
        ensure_not_cancelled(cancellation)?;
        if info.has_audio {
            audio::ensure_available()?;
        }
        ensure_not_cancelled(cancellation)?;
        let source = VideoSource {
            path: path.to_path_buf(),
            source_revision,
            info,
        };
        Self::open_source_cancellable(source, picker, cell_area, 0.0, cancellation)
    }

    pub(crate) fn open_source(
        source: VideoSource,
        picker: &Picker,
        cell_area: Rect,
        position: f64,
    ) -> Result<VideoPlayer, VideoUnavailable> {
        Self::open_source_cancellable(
            source,
            picker,
            cell_area,
            position,
            &reef_io::CancellationToken::default(),
        )
    }

    pub(crate) fn open_source_cancellable(
        source: VideoSource,
        picker: &Picker,
        cell_area: Rect,
        position: f64,
        cancellation: &reef_io::CancellationToken,
    ) -> Result<VideoPlayer, VideoUnavailable> {
        ensure_not_cancelled(cancellation)?;
        let VideoSource {
            path,
            source_revision,
            info,
        } = source;
        let (frame_px, area) = frame_size(info, picker, cell_area);
        let fps = playback_fps(info.fps, frame_px);
        let audio = info
            .has_audio
            .then(|| AudioPlayer::new(path.clone(), position));

        let mut player = VideoPlayer {
            path,
            source_revision,
            info,
            fps,
            cell_area: area,
            frame_px,
            decoder: None,
            encoder: None,
            frame: None,
            position,
            frame_position: position,
            playing: false,
            ended: false,
            wall_clock: None,
            audio,
            playback_error: None,
        };
        player.start_decoder(position)?;
        // Pull the opening frame synchronously so the card never flashes an
        // empty box between selection and first paint.
        player.await_first_frame(picker, cancellation)?;
        ensure_not_cancelled(cancellation)?;
        player.encoder = Some(FrameEncoder::spawn(
            picker.clone(),
            player.frame_px,
            player.cell_area,
        )?);
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

    /// Convert a timeline ratio into a position that still has a decodable
    /// frame. Seeking to the container's exact duration would start ffmpeg
    /// after the final frame, so the right edge lands one output frame back.
    pub(crate) fn seek_position(&self, ratio: f64) -> Option<f64> {
        seek_position(self.info.duration, self.fps, ratio)
    }

    /// The frame to draw, and the cell area it occupies.
    pub fn frame(&self) -> Option<(&Protocol, Rect)> {
        self.frame.as_ref().map(|frame| (frame, self.cell_area))
    }

    pub(crate) fn set_playing(&mut self, playing: bool) -> Result<(), VideoUnavailable> {
        self.set_playing_at(playing, Instant::now())
    }

    fn set_playing_at(&mut self, playing: bool, now: Instant) -> Result<(), VideoUnavailable> {
        let playing = playing && !self.ended;
        if playing
            && !self.playing
            && let Some(audio) = self.audio.as_mut()
        {
            audio.play(self.position)?;
        } else if !playing
            && self.playing
            && let Some(audio) = self.audio.as_mut()
        {
            self.position = audio.pause_at(now);
        } else if playing && !self.playing {
            self.wall_clock = Some(PlaybackClock::new(self.position, now));
        } else if !playing
            && self.playing
            && let Some(clock) = self.wall_clock.take()
        {
            self.position = self.clamp_position(clock.position_at(now));
        }
        self.playing = playing;
        Ok(())
    }

    /// Toggle play/pause. Returns `false` when the clip has ended, or when an
    /// audio process could not be started.
    pub fn toggle(&mut self) -> bool {
        if self.ended {
            return false;
        }
        self.set_playing(!self.playing).is_ok()
    }

    pub fn pause(&mut self) {
        let _ = self.set_playing(false);
    }

    pub(crate) fn take_playback_error(&mut self) -> Option<VideoUnavailable> {
        self.playback_error.take()
    }

    pub fn matches_area(&self, picker: &Picker, cell_area: Rect) -> bool {
        let (frame_px, area) = frame_size(self.info, picker, cell_area);
        frame_px == self.frame_px && area == self.cell_area
    }

    /// Advance playback. Returns whether the visible frame changed, which the
    /// caller turns into a redraw.
    pub fn tick(&mut self, now: Instant, _picker: &Picker) -> bool {
        let encoded = self.poll_encoded_frame();
        if !self.playing || self.ended {
            return encoded;
        }
        if self.audio.is_some() {
            return self.tick_to_audio(now) || encoded;
        }
        let Some(clock) = self.wall_clock else {
            return encoded;
        };
        self.position = self.clamp_position(clock.position_at(now));
        let advance = self.advance_frames_to(self.position);
        if advance.decoder_finished {
            return self.finish();
        }
        encoded
    }

    /// Settle into the ended state: no decoder, no schedule, position parked
    /// at the duration. The last encoded frame stays on screen. Always
    /// reports `true` so the card redraws with its replay hint.
    fn finish(&mut self) -> bool {
        self.ended = true;
        self.playing = false;
        self.wall_clock = None;
        self.decoder = None;
        self.audio = None;
        if let Some(duration) = self.info.duration {
            self.position = duration;
            self.frame_position = duration;
        }
        true
    }

    fn tick_to_audio(&mut self, now: Instant) -> bool {
        let Some(audio) = self.audio.as_mut() else {
            return false;
        };
        let poll = audio.poll_at(now);
        self.position = self.clamp_position(poll.position);

        let advance = self.advance_frames_to(self.position);
        match poll.state {
            AudioPollState::Playing => {}
            AudioPollState::Ended => {
                if advance.decoder_finished {
                    return self.finish();
                }
                self.audio = None;
                self.wall_clock = Some(PlaybackClock::new(self.position, now));
            }
            AudioPollState::Failed(error) => {
                self.playing = false;
                self.decoder = None;
                self.audio = None;
                self.playback_error = Some(VideoUnavailable::Unreadable(error));
                return true;
            }
        }
        false
    }

    /// Consume every frame whose presentation time is due, but encode only
    /// the newest one. Dropping intermediate raw frames is what lets video
    /// catch up without delaying the audio clock.
    fn advance_frames_to(&mut self, target: f64) -> AdvanceResult {
        let due = frames_due(self.frame_position, target, self.fps);
        let mut latest = None;
        let mut decoder_finished = self.decoder.is_none();
        for _ in 0..due {
            match self.take_frame() {
                FramePull::Frame(bytes) => {
                    self.frame_position += 1.0 / self.fps;
                    latest = Some(bytes);
                }
                FramePull::Pending => break,
                FramePull::Finished => {
                    decoder_finished = true;
                    break;
                }
            }
        }
        if let Some(bytes) = latest {
            self.queue_encode(bytes);
        }
        if decoder_finished {
            self.decoder = None;
        }
        AdvanceResult { decoder_finished }
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
            // Audio has its own ffplay process and clock channel.
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
    fn await_first_frame(
        &mut self,
        picker: &Picker,
        cancellation: &reef_io::CancellationToken,
    ) -> Result<(), VideoUnavailable> {
        let Some(decoder) = self.decoder.as_ref() else {
            return Err(VideoUnavailable::Unreadable("decoder unavailable".into()));
        };
        let bytes = first_frame_bytes(&decoder.frames, cancellation, FIRST_FRAME_TIMEOUT)?;
        self.frame = encode_frame(picker, self.frame_px, self.cell_area, bytes);
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

    fn queue_encode(&mut self, bytes: Vec<u8>) {
        if let Some(encoder) = self.encoder.as_mut() {
            encoder.submit(bytes);
        }
    }

    fn poll_encoded_frame(&mut self) -> bool {
        let Some(frame) = self.encoder.as_mut().and_then(FrameEncoder::poll) else {
            return false;
        };
        self.frame = Some(frame);
        true
    }

    fn clamp_position(&self, position: f64) -> f64 {
        match self.info.duration {
            Some(duration) => position.min(duration),
            None => position,
        }
    }
}

fn ensure_not_cancelled(cancellation: &reef_io::CancellationToken) -> Result<(), VideoUnavailable> {
    if cancellation.is_cancelled() {
        Err(VideoUnavailable::Cancelled)
    } else {
        Ok(())
    }
}

/// Wrap raw RGBA bytes as an image and encode them for the terminal. The
/// buffer is already the exact frame the panel shows, so no resize happens.
fn encode_frame(
    picker: &Picker,
    frame_px: (u32, u32),
    cell_area: Rect,
    bytes: Vec<u8>,
) -> Option<Protocol> {
    let (width, height) = frame_px;
    let buffer = RgbaImage::from_raw(width, height, bytes)?;
    let image = DynamicImage::ImageRgba8(buffer);
    match picker.protocol_type() {
        ProtocolType::Kitty => Kitty::new(image, cell_area, KITTY_VIDEO_ID, false)
            .ok()
            .map(Protocol::Kitty),
        ProtocolType::Iterm2 => Iterm2::new(image, cell_area, false)
            .ok()
            .map(Protocol::ITerm2),
        ProtocolType::Halfblocks | ProtocolType::Sixel => None,
    }
}

fn first_frame_bytes(
    frames: &Receiver<Vec<u8>>,
    cancellation: &reef_io::CancellationToken,
    timeout: Duration,
) -> Result<Vec<u8>, VideoUnavailable> {
    let deadline = Instant::now() + timeout;
    loop {
        ensure_not_cancelled(cancellation)?;
        let now = Instant::now();
        if now >= deadline {
            return Err(VideoUnavailable::Unreadable(
                "timed out waiting for first frame".into(),
            ));
        }
        let wait = deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(50));
        match frames.recv_timeout(wait) {
            Ok(bytes) => return Ok(bytes),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(VideoUnavailable::Unreadable(
                    "ffmpeg produced no frames".into(),
                ));
            }
        }
    }
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

fn frames_due(frame_position: f64, target: f64, fps: f64) -> usize {
    if !fps.is_finite() || fps <= 0.0 || !target.is_finite() {
        return 0;
    }
    let interval = 1.0 / fps;
    let delta = target + interval / 2.0 - frame_position;
    if delta < interval {
        return 0;
    }
    (delta / interval).floor() as usize
}

fn seek_position(duration: Option<f64>, fps: f64, ratio: f64) -> Option<f64> {
    let duration = duration.filter(|duration| duration.is_finite() && *duration > 0.0)?;
    let last_frame = (duration - 1.0 / fps).max(0.0);
    Some((duration * ratio.clamp(0.0, 1.0)).min(last_frame))
}

/// The ffmpeg filter chain: drop to playback frame rate, then scale to the
/// display-correct square-pixel geometry calculated from ffprobe metadata.
/// Every decoded frame therefore has the same fixed byte size without a
/// second resize in the terminal encoder.
fn filter_chain(width: u32, height: u32, fps: f64) -> String {
    format!("fps={fps:.4},scale={width}:{height}:flags=fast_bilinear,setsar=1,format=rgba")
}

/// Pick the frame pixel size and the cell area that encloses it. Pixel size
/// stays at or below source resolution, panel capacity, and the terminal wire
/// budget; only placement rounds up to whole cells, so 1× is never exceeded.
fn frame_size(info: VideoInfo, picker: &Picker, cell_area: Rect) -> ((u32, u32), Rect) {
    let (font_w, font_h) = picker.font_size();
    let font_w = font_w.max(1) as u32;
    let font_h = font_h.max(1) as u32;

    let max_cols = cell_area.width.max(1) as u32;
    let max_rows = cell_area.height.max(1) as u32;
    let max_px_w = max_cols * font_w;
    let max_px_h = max_rows * font_h;

    let src_w = info.width.max(1);
    let src_h = info.height.max(1);
    let source_pixels = u64::from(src_w) * u64::from(src_h);
    let max_frame_pixels = max_frame_pixels();
    let budget_scale = (max_frame_pixels as f64 / source_pixels as f64)
        .min(1.0)
        .sqrt();
    let scale = (max_px_w as f64 / src_w as f64)
        .min(max_px_h as f64 / src_h as f64)
        .min(budget_scale)
        .min(1.0);
    let fit_w = ((src_w as f64 * scale).floor() as u32).max(1);
    let fit_h = ((src_h as f64 * scale).floor() as u32).max(1);

    let cols = fit_w.div_ceil(font_w).clamp(1, max_cols);
    let rows = fit_h.div_ceil(font_h).clamp(1, max_rows);

    let area = Rect::new(cell_area.x, cell_area.y, cols as u16, rows as u16);
    ((fit_w, fit_h), area)
}

fn max_frame_pixels() -> u64 {
    MAX_FRAME_WIRE_BYTES / ESTIMATED_WIRE_BYTES_PER_PIXEL
}

fn estimated_frame_wire_bytes((width, height): (u32, u32)) -> u64 {
    u64::from(width) * u64::from(height) * ESTIMATED_WIRE_BYTES_PER_PIXEL
}

fn playback_fps(source_fps: Option<f64>, frame_px: (u32, u32)) -> f64 {
    let ceiling = std::env::var("REEF_VIDEO_FPS")
        .ok()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|fps| fps.is_finite())
        .map(|fps| fps.clamp(1.0, 60.0))
        .unwrap_or(DEFAULT_MAX_FPS);
    let wire_fps =
        MAX_VIDEO_WIRE_BYTES_PER_SECOND as f64 / estimated_frame_wire_bytes(frame_px).max(1) as f64;
    let ceiling = ceiling.min(wire_fps.max(1.0));
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

/// Ask ffprobe for the source's display geometry, frame rate, and duration.
fn probe(path: &Path) -> Result<VideoInfo, VideoUnavailable> {
    let output = Command::new("ffprobe")
        .args(["-v", "error"])
        .args(["-select_streams", "v:0"])
        .args([
            "-show_entries",
            "stream=width,height,r_frame_rate,sample_aspect_ratio:stream_side_data=rotation",
        ])
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
    let mut info = parse_probe(&String::from_utf8_lossy(&output.stdout))?;
    info.has_audio = probe_has_audio(path)?;
    Ok(info)
}

fn probe_has_audio(path: &Path) -> Result<bool, VideoUnavailable> {
    let output = Command::new("ffprobe")
        .args(["-v", "error"])
        .args(["-select_streams", "a:0"])
        .args(["-show_entries", "stream=index"])
        .args(["-of", "csv=p=0"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|_| VideoUnavailable::NoFfmpeg)?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(VideoUnavailable::Unreadable(first_line(&detail)));
    }
    Ok(!output.stdout.iter().all(u8::is_ascii_whitespace))
}

/// Parse ffprobe's `key=value` lines. Width and height are required — a file
/// without them has no video stream we can show.
fn parse_probe(text: &str) -> Result<VideoInfo, VideoUnavailable> {
    let mut width = None;
    let mut height = None;
    let mut fps = None;
    let mut duration = None;
    let mut sample_aspect_ratio = None;
    let mut rotation = None;

    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "width" => width = value.parse::<u32>().ok(),
            "height" => height = value.parse::<u32>().ok(),
            "r_frame_rate" => fps = parse_rational(value),
            "sample_aspect_ratio" => sample_aspect_ratio = parse_rational(value),
            "rotation" => rotation = value.parse::<f64>().ok().filter(|value| value.is_finite()),
            "duration" => duration = value.parse::<f64>().ok().filter(|d| d.is_finite()),
            _ => {}
        }
    }

    match (width, height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => {
            let (width, height) = display_dimensions(width, height, sample_aspect_ratio, rotation);
            Ok(VideoInfo {
                width,
                height,
                fps,
                duration,
                has_audio: false,
            })
        }
        _ => Err(VideoUnavailable::Unreadable("no video stream".into())),
    }
}

/// Convert coded dimensions into square-pixel display dimensions without
/// exceeding the source's oriented pixel bounds. ffmpeg applies rotation
/// metadata before our filter chain, so the target geometry must make the
/// same quarter-turn decision. Anamorphic sources are fitted down on one axis
/// instead of being upscaled on the other.
fn display_dimensions(
    width: u32,
    height: u32,
    sample_aspect_ratio: Option<f64>,
    rotation: Option<f64>,
) -> (u32, u32) {
    let sar = sample_aspect_ratio
        .filter(|ratio| ratio.is_finite() && *ratio > 0.0)
        .unwrap_or(1.0);
    let swaps_axes = rotation.is_some_and(|degrees| {
        let normalized = degrees.rem_euclid(180.0);
        (normalized - 90.0).abs() < 0.01
    });
    let (bound_w, bound_h, display_aspect) = if swaps_axes {
        (height, width, height as f64 / (width as f64 * sar))
    } else {
        (width, height, width as f64 * sar / height as f64)
    };
    let bounds_aspect = bound_w as f64 / bound_h as f64;
    if bounds_aspect > display_aspect {
        let fitted_w = (bound_h as f64 * display_aspect).floor().max(1.0) as u32;
        (fitted_w.min(bound_w), bound_h)
    } else {
        let fitted_h = (bound_w as f64 / display_aspect).floor().max(1.0) as u32;
        (bound_w, fitted_h.min(bound_h))
    }
}

/// ffprobe reports frame rates as exact rationals (`30000/1001`).
fn parse_rational(value: &str) -> Option<f64> {
    let (num, den) = value.split_once('/').or_else(|| value.split_once(':'))?;
    let num: f64 = num.trim().parse().ok()?;
    let den: f64 = den.trim().parse().ok()?;
    let ratio = num / den;
    (den != 0.0 && num > 0.0 && ratio.is_finite()).then_some(ratio)
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
            has_audio: false,
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
        assert!(!parsed.has_audio);
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
    fn probe_uses_rotated_display_dimensions() {
        let parsed = parse_probe(
            "width=320\nheight=240\nsample_aspect_ratio=1:1\nrotation=90\n\
             r_frame_rate=30/1\n",
        )
        .expect("rotated dimensions should parse");

        assert_eq!((parsed.width, parsed.height), (240, 320));
    }

    #[test]
    fn probe_normalizes_anamorphic_pixels_without_upscaling() {
        let parsed =
            parse_probe("width=720\nheight=576\nsample_aspect_ratio=16:15\nr_frame_rate=25/1\n")
                .expect("anamorphic dimensions should parse");

        assert_eq!((parsed.width, parsed.height), (720, 540));
    }

    #[test]
    fn playback_fps_is_capped_but_never_exceeds_source() {
        let small_frame = (640, 360);
        assert_eq!(playback_fps(Some(60.0), small_frame), DEFAULT_MAX_FPS);
        assert_eq!(playback_fps(Some(8.0), small_frame), 8.0);
        assert_eq!(playback_fps(None, small_frame), DEFAULT_MAX_FPS);
    }

    #[test]
    fn playback_fps_respects_the_terminal_bandwidth_budget() {
        let fps = playback_fps(Some(60.0), (1024, 576));
        let bytes_per_second = fps * estimated_frame_wire_bytes((1024, 576)) as f64;

        assert!(bytes_per_second <= MAX_VIDEO_WIRE_BYTES_PER_SECOND as f64);
        assert!(fps < DEFAULT_MAX_FPS);
    }

    #[test]
    fn audio_target_selects_due_frames_with_half_frame_tolerance() {
        assert_eq!(frames_due(0.0, 0.02, 15.0), 0);
        assert_eq!(frames_due(0.0, 0.04, 15.0), 1);
        assert_eq!(frames_due(0.0, 0.20, 15.0), 3);
    }

    #[test]
    fn playback_clock_tracks_elapsed_wall_time() {
        let now = Instant::now();
        let clock = PlaybackClock::new(2.5, now);

        assert_eq!(clock.position_at(now + Duration::from_millis(750)), 3.25);
    }

    #[test]
    fn seek_position_maps_the_timeline_midpoint() {
        assert_eq!(seek_position(Some(10.0), 15.0, 0.5), Some(5.0));
    }

    #[test]
    fn seek_position_keeps_the_right_edge_decodable() {
        let target = seek_position(Some(10.0), 15.0, 1.0).expect("seek target");
        assert!((target - (10.0 - 1.0 / 15.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn frame_size_is_enclosed_by_whole_cells_within_the_panel() {
        let picker = picker_with_cells((10, 20));
        let area = Rect::new(3, 4, 40, 20);
        let ((px_w, px_h), cells) = frame_size(info(1920, 1080), &picker, area);

        assert!(cells.width <= area.width && cells.height <= area.height);
        assert_eq!((cells.x, cells.y), (area.x, area.y));
        assert!(px_w <= u32::from(cells.width) * 10);
        assert!(px_h <= u32::from(cells.height) * 20);
        assert!(px_w > u32::from(cells.width.saturating_sub(1)) * 10);
        assert!(px_h > u32::from(cells.height.saturating_sub(1)) * 20);
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
    fn frame_size_keeps_non_cell_aligned_sources_at_strict_one_x() {
        let picker = picker_with_cells((10, 20));
        let ((px_w, px_h), cells) = frame_size(info(641, 361), &picker, Rect::new(0, 0, 100, 100));

        assert_eq!((px_w, px_h), (641, 361));
        assert_eq!((cells.width, cells.height), (65, 19));
    }

    #[test]
    fn frame_size_caps_large_sources_to_the_wire_budget() {
        let picker = picker_with_cells((10, 20));
        let (frame_px, _) = frame_size(info(3840, 2160), &picker, Rect::new(0, 0, 400, 200));

        assert!(u64::from(frame_px.0) * u64::from(frame_px.1) <= max_frame_pixels());
        assert!(estimated_frame_wire_bytes(frame_px) <= MAX_FRAME_WIRE_BYTES);
    }

    #[test]
    fn frame_size_survives_a_degenerate_panel() {
        let picker = picker_with_cells((10, 20));
        let ((px_w, px_h), cells) = frame_size(info(1920, 1080), &picker, Rect::new(0, 0, 0, 0));
        assert!(px_w > 0 && px_h > 0);
        assert_eq!((cells.width, cells.height), (1, 1));
    }

    #[test]
    fn filter_chain_scales_to_fixed_square_pixel_dimensions() {
        let chain = filter_chain(320, 240, 15.0);
        assert!(chain.contains("fps=15.0000"));
        assert!(chain.contains("scale=320:240:flags=fast_bilinear"));
        assert!(chain.contains("setsar=1"));
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
        let (tx, rx) = sync_channel(1);
        drop(tx);
        assert_eq!(
            first_frame_bytes(
                &rx,
                &reef_io::CancellationToken::default(),
                Duration::from_secs(1),
            ),
            Err(VideoUnavailable::Unreadable(
                "ffmpeg produced no frames".into()
            ))
        );
    }

    #[test]
    fn timed_out_first_frame_is_unreadable() {
        let (_tx, rx) = sync_channel(1);
        assert_eq!(
            first_frame_bytes(&rx, &reef_io::CancellationToken::default(), Duration::ZERO,),
            Err(VideoUnavailable::Unreadable(
                "timed out waiting for first frame".into()
            ))
        );
    }

    #[test]
    fn cancelled_open_stops_before_spawning_ffmpeg() {
        let cancellation = reef_io::CancellationToken::default();
        cancellation.cancel();

        assert!(matches!(
            VideoPlayer::open_cancellable(
                Path::new("missing.mp4"),
                0,
                &picker_with_cells((10, 20)),
                Rect::new(0, 0, 40, 12),
                &cancellation,
            ),
            Err(VideoUnavailable::Cancelled)
        ));
    }
}
