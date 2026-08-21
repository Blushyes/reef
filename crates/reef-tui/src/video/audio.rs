//! Audio subprocess and clock IPC for inline video playback.

use super::{VideoUnavailable, kill_and_reap};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Instant;

pub(super) struct AudioPlayer {
    path: PathBuf,
    process: Option<AudioProcess>,
    sample: ClockSample,
}

struct AudioProcess {
    child: Option<Child>,
    updates: Receiver<AudioUpdate>,
    stream_closed: bool,
    exit_detail: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct ClockSample {
    position: f64,
    observed_at: Instant,
    advancing: bool,
}

enum AudioUpdate {
    Clock(f64),
    StreamClosed(Option<String>),
}

#[derive(Debug, Clone)]
pub(super) struct AudioPoll {
    pub position: f64,
    pub state: AudioPollState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AudioPollState {
    Playing,
    Ended,
    Failed(String),
}

impl Drop for AudioProcess {
    fn drop(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        kill_and_reap(child, "reef-audio-reap");
    }
}

impl AudioPlayer {
    pub(super) fn new(path: PathBuf, position: f64) -> Self {
        Self {
            path,
            process: None,
            sample: ClockSample {
                position,
                observed_at: Instant::now(),
                advancing: false,
            },
        }
    }

    pub(super) fn play(&mut self, position: f64) -> Result<(), VideoUnavailable> {
        self.process = None;

        let mut command = ffplay_command(&self.path, position);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|_| VideoUnavailable::NoFfplay)?;
        let Some(stderr) = child.stderr.take() else {
            kill_and_reap(child, "reef-audio-reap");
            return Err(VideoUnavailable::Unreadable("ffplay stderr".into()));
        };

        let (tx, rx) = channel();
        let reader = std::thread::Builder::new()
            .name("reef-audio-clock".into())
            .spawn(move || read_audio_clock(stderr, position, tx));
        if let Err(error) = reader {
            kill_and_reap(child, "reef-audio-reap");
            return Err(VideoUnavailable::Unreadable(error.to_string()));
        }

        self.sample = ClockSample {
            position,
            observed_at: Instant::now(),
            advancing: false,
        };
        self.process = Some(AudioProcess {
            child: Some(child),
            updates: rx,
            stream_closed: false,
            exit_detail: None,
        });
        Ok(())
    }

    pub(super) fn poll_at(&mut self, now: Instant) -> AudioPoll {
        let mut state = AudioPollState::Playing;
        if let Some(process) = self.process.as_mut() {
            while let Ok(update) = process.updates.try_recv() {
                match update {
                    AudioUpdate::Clock(position) => {
                        self.sample = ClockSample {
                            position,
                            observed_at: now,
                            advancing: true,
                        };
                    }
                    AudioUpdate::StreamClosed(detail) => {
                        process.stream_closed = true;
                        process.exit_detail = detail;
                    }
                }
            }
            if process.stream_closed {
                state = match process.child.as_mut().map(Child::try_wait) {
                    Some(Ok(Some(status))) => {
                        process.child = None;
                        classify_audio_exit(status.success(), process.exit_detail.take())
                    }
                    Some(Err(error)) => AudioPollState::Failed(error.to_string()),
                    Some(Ok(None)) | None => AudioPollState::Playing,
                };
            }
        }
        AudioPoll {
            position: self.sample.position_at(now),
            state,
        }
    }

    pub(super) fn pause_at(&mut self, now: Instant) -> f64 {
        let poll = self.poll_at(now);
        self.process = None;
        self.sample = ClockSample {
            position: poll.position,
            observed_at: now,
            advancing: false,
        };
        poll.position
    }
}

fn ffplay_command(path: &std::path::Path, position: f64) -> Command {
    let mut command = Command::new("ffplay");
    command
        .args(["-hide_banner", "-loglevel", "info", "-stats"])
        .args(["-nodisp", "-autoexit", "-vn"]);
    if position > 0.0 {
        command
            .args(["-ss", &format!("{position:.6}")])
            .args(["-af", "asetpts=PTS-STARTPTS"]);
    }
    command.arg(path);
    command
}

impl ClockSample {
    fn position_at(self, now: Instant) -> f64 {
        if self.advancing {
            self.position
                + now
                    .saturating_duration_since(self.observed_at)
                    .as_secs_f64()
        } else {
            self.position
        }
    }
}

pub(super) fn ensure_available() -> Result<(), VideoUnavailable> {
    let status = Command::new("ffplay")
        .args(["-version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| VideoUnavailable::NoFfplay)?;
    if status.success() {
        Ok(())
    } else {
        Err(VideoUnavailable::NoFfplay)
    }
}

/// Forward ffplay's audio-master clock from stderr into the TUI process.
/// Status records are carriage-return delimited because ffplay rewrites one
/// terminal line while it plays.
fn read_audio_clock(stderr: std::process::ChildStderr, start: f64, tx: Sender<AudioUpdate>) {
    read_audio_clock_stream(BufReader::new(stderr), start, tx);
}

fn read_audio_clock_stream(mut reader: impl BufRead, start: f64, tx: Sender<AudioUpdate>) {
    let mut record = Vec::new();
    let mut last_detail = None;
    loop {
        record.clear();
        match reader.read_until(b'\r', &mut record) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let Some(relative) = parse_ffplay_clock(&record) else {
                    if let Some(detail) = audio_output_detail(&record) {
                        last_detail = Some(detail);
                    }
                    continue;
                };
                if tx
                    .send(AudioUpdate::Clock(start + relative.max(0.0)))
                    .is_err()
                {
                    return;
                }
            }
        }
    }
    let _ = tx.send(AudioUpdate::StreamClosed(last_detail));
}

fn classify_audio_exit(success: bool, detail: Option<String>) -> AudioPollState {
    if success {
        AudioPollState::Ended
    } else {
        AudioPollState::Failed(detail.unwrap_or_else(|| "ffplay exited unsuccessfully".into()))
    }
}

fn audio_output_detail(record: &[u8]) -> Option<String> {
    String::from_utf8_lossy(record)
        .lines()
        .map(|line| line.trim().trim_start_matches("\u{1b}[2K").trim())
        .rfind(|line| !line.is_empty())
        .map(str::to_string)
}

fn parse_ffplay_clock(record: &[u8]) -> Option<f64> {
    let text = String::from_utf8_lossy(record);
    for line in text.lines().rev() {
        let line = line.trim().trim_start_matches("\u{1b}[2K").trim();
        let mut fields = line.split_whitespace();
        let Some(clock) = fields.next().and_then(|field| field.parse::<f64>().ok()) else {
            continue;
        };
        if fields.next() == Some("M-A:") && clock.is_finite() {
            return Some(clock);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ffplay_audio_master_status_yields_clock() {
        assert_eq!(
            parse_ffplay_clock(b"\x1b[2K   1.27 M-A:  0.000 fd=0 aq=3KB\r"),
            Some(1.27)
        );
    }

    #[test]
    fn ffplay_non_clock_output_is_ignored() {
        assert_eq!(parse_ffplay_clock(b"Duration: 00:00:04.00\n"), None);
        assert_eq!(parse_ffplay_clock(b"nan M-A: nan fd=0\r"), None);
        assert_eq!(parse_ffplay_clock(b"1.00 A-V: 0.000 fd=0\r"), None);
    }

    #[test]
    fn audio_clock_stream_sends_absolute_positions_and_end() {
        let input = b"metadata\n\r  0.10 M-A: 0.000\r  0.25 M-A: 0.000\r";
        let (tx, rx) = channel();

        read_audio_clock_stream(&input[..], 2.0, tx);

        assert!(matches!(rx.recv(), Ok(AudioUpdate::Clock(value)) if value == 2.1));
        assert!(matches!(rx.recv(), Ok(AudioUpdate::Clock(value)) if value == 2.25));
        assert!(matches!(rx.recv(), Ok(AudioUpdate::StreamClosed(_))));
    }

    #[test]
    fn audio_clock_waits_for_first_process_sample_then_extrapolates() {
        let now = Instant::now();
        let waiting = ClockSample {
            position: 2.0,
            observed_at: now,
            advancing: false,
        };
        let advancing = ClockSample {
            advancing: true,
            ..waiting
        };

        assert_eq!(waiting.position_at(now + Duration::from_secs(1)), 2.0);
        assert_eq!(advancing.position_at(now + Duration::from_secs(1)), 3.0);
    }

    #[test]
    fn seeking_uses_input_seek_without_decoding_the_prefix() {
        let command = ffplay_command(std::path::Path::new("clip.mp4"), 12.5);
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(args.windows(2).any(|pair| pair == ["-ss", "12.500000"]));
        assert!(!args.iter().any(|arg| arg.contains("atrim")));
    }

    #[test]
    fn unsuccessful_audio_exit_preserves_ffplay_detail() {
        assert_eq!(
            classify_audio_exit(false, Some("audio device unavailable".into())),
            AudioPollState::Failed("audio device unavailable".into())
        );
    }

    #[test]
    fn successful_audio_exit_is_natural_end() {
        assert_eq!(classify_audio_exit(true, None), AudioPollState::Ended);
    }
}
