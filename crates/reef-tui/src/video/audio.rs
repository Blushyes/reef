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
}

#[derive(Debug, Clone, Copy)]
struct ClockSample {
    position: f64,
    observed_at: Instant,
    advancing: bool,
}

enum AudioUpdate {
    Clock(f64),
    Ended,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct AudioPoll {
    pub position: f64,
    pub ended: bool,
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

        let mut command = Command::new("ffplay");
        command
            .args(["-hide_banner", "-loglevel", "info", "-stats"])
            .args(["-nodisp", "-autoexit", "-vn"]);
        if position > 0.0 {
            command.args([
                "-af",
                &format!("atrim=start={position:.6},asetpts=PTS-STARTPTS"),
            ]);
        }
        command
            .arg(&self.path)
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
        });
        Ok(())
    }

    pub(super) fn poll_at(&mut self, now: Instant) -> AudioPoll {
        let mut ended = false;
        if let Some(process) = self.process.as_ref() {
            while let Ok(update) = process.updates.try_recv() {
                match update {
                    AudioUpdate::Clock(position) => {
                        self.sample = ClockSample {
                            position,
                            observed_at: now,
                            advancing: true,
                        };
                    }
                    AudioUpdate::Ended => ended = true,
                }
            }
        }
        AudioPoll {
            position: self.sample.position_at(now),
            ended,
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
    loop {
        record.clear();
        match reader.read_until(b'\r', &mut record) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let Some(relative) = parse_ffplay_clock(&record) else {
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
    let _ = tx.send(AudioUpdate::Ended);
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
        assert!(matches!(rx.recv(), Ok(AudioUpdate::Ended)));
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
}
