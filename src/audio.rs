//! Audio capture through `parec`: one process per source, kept running for the
//! whole life of the app so the meters work before and after a recording too.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
/// 20 ms of s16le audio.
const CHUNK_BYTES: usize = (RATE / 50 * 2 * CHANNELS) as usize;
const FRAME_BYTES: usize = (2 * CHANNELS) as usize;
/// A read that waits this long means the device gave nothing in between.
const STALL: Duration = Duration::from_millis(250);
/// Three seconds of 20 ms peaks.
pub const HISTORY: usize = 150;
const FLOOR_DB: f64 = -60.0;

struct Inner {
    levels: VecDeque<f32>,
    recording: Option<Recording>,
    /// While paused the meters keep running but nothing is written.
    paused: bool,
}

struct Recording {
    file: BufWriter<File>,
    /// Where the recording is, so audio that never came becomes silence.
    timeline: Timeline,
}

#[derive(Clone)]
pub struct Source {
    inner: Arc<Mutex<Inner>>,
}

impl Source {
    /// Starts capturing `device`, a PulseAudio source name such as `@DEFAULT_MONITOR@`.
    pub fn spawn(device: &'static str) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            levels: VecDeque::from(vec![0.0; HISTORY]),
            recording: None,
            paused: false,
        }));
        let shared = inner.clone();
        thread::spawn(move || {
            loop {
                capture(device, &shared);
                // parec exits when the device goes away; try again.
                thread::sleep(Duration::from_secs(1));
            }
        });
        Source { inner }
    }

    /// Tees the raw stream (s16le, RATE, CHANNELS) into `path` from now on.
    pub fn start_recording(&self, path: &Path) -> std::io::Result<()> {
        let file = BufWriter::new(File::create(path)?);
        let mut inner = self.inner.lock().unwrap();
        inner.recording = Some(Recording {
            file,
            timeline: Timeline::new(Instant::now()),
        });
        inner.paused = false;
        Ok(())
    }

    pub fn set_paused(&self, paused: bool) {
        let mut inner = self.inner.lock().unwrap();
        inner.paused = paused;
        if let Some(recording) = inner.recording.as_mut() {
            recording.timeline.set_paused(paused, Instant::now());
        }
    }

    pub fn stop_recording(&self) {
        if let Some(mut recording) = self.inner.lock().unwrap().recording.take() {
            let _ = recording.file.flush();
        }
    }

    pub fn levels(&self) -> Vec<f32> {
        self.inner.lock().unwrap().levels.iter().copied().collect()
    }

    /// The loudest of the last `n` peaks, so a short burst is not missed by a slower reader.
    pub fn recent_peak(&self, n: usize) -> f32 {
        let inner = self.inner.lock().unwrap();
        inner
            .levels
            .iter()
            .rev()
            .take(n)
            .copied()
            .fold(0.0, f32::max)
    }
}

fn capture(device: &str, shared: &Mutex<Inner>) {
    let Ok(mut child) = Command::new("parec")
        .args([
            "--raw",
            "--format=s16le",
            &format!("--rate={RATE}"),
            &format!("--channels={CHANNELS}"),
            "--latency-msec=20",
            "-d",
            device,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut buf = vec![0u8; CHUNK_BYTES];
    // So a crash loses at most a second: flush every second, and push it to
    // the disk itself every half minute in case the machine goes down too.
    let mut chunks: u64 = 0;
    loop {
        let asked = Instant::now();
        if stdout.read_exact(&mut buf).is_err() {
            break;
        }
        chunks += 1;
        let now = Instant::now();
        let peak = buf
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs())
            .max()
            .unwrap_or(0) as f32
            / 32768.0;
        let mut guard = shared.lock().unwrap();
        let inner = &mut *guard;
        inner.levels.pop_front();
        inner.levels.push_back(peak);
        if let Some(Recording { file, timeline }) = inner.recording.as_mut() {
            timeline.heard(now.saturating_duration_since(asked), chunks == 1);
            if inner.paused {
                continue;
            }
            let missing = timeline.missing_before(now);
            let _ = write_silence(file, missing);
            timeline.wrote(missing + (CHUNK_BYTES / FRAME_BYTES) as u64);
            let _ = file.write_all(&buf);
            if chunks.is_multiple_of(50) {
                let _ = file.flush();
            }
            if chunks.is_multiple_of(1500) {
                let _ = file.get_ref().sync_data();
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Where a recording is: how much audio it should hold by now, against how
/// much it does. A track that falls behind, because the audio stopped coming
/// in for a while, catches up with silence, so it stays in step with the other
/// one. Time spent paused does not count.
struct Timeline {
    /// When the recording started, moved on by every pause.
    origin: Instant,
    paused_at: Option<Instant>,
    /// Frames in the file, silence included.
    frames: u64,
    /// Set when nothing came for a while, until the next chunk is written.
    quiet: bool,
}

impl Timeline {
    fn new(at: Instant) -> Self {
        Timeline {
            origin: at,
            paused_at: None,
            frames: 0,
            quiet: false,
        }
    }

    fn set_paused(&mut self, paused: bool, at: Instant) {
        if paused {
            self.paused_at.get_or_insert(at);
        } else if let Some(began) = self.paused_at.take() {
            self.origin += at.saturating_duration_since(began);
        }
    }

    /// Notes a chunk that came after a read that `waited`, the `first` of a
    /// `parec`, paused or not. Nothing came for a while when it is a new
    /// `parec` (the old one exited, or the device was not there yet), or when
    /// the stream was held for seconds, as by a Bluetooth headset switching to
    /// its headset profile. A read that returns at once finds audio that was
    /// only waiting for us, and the track catches up by itself.
    fn heard(&mut self, waited: Duration, first: bool) {
        self.quiet |= first || waited > STALL;
    }

    /// The frames of silence before a 20 ms chunk written at `at`: what the
    /// track is short of, after a quiet spell.
    fn missing_before(&self, at: Instant) -> u64 {
        if !self.quiet {
            return 0;
        }
        let due = frames_in(at.saturating_duration_since(self.origin));
        due.saturating_sub(self.frames + (CHUNK_BYTES / FRAME_BYTES) as u64)
    }

    fn wrote(&mut self, frames: u64) {
        self.frames += frames;
        self.quiet = false;
    }
}

fn frames_in(duration: Duration) -> u64 {
    (duration.as_micros() * u128::from(RATE) / 1_000_000) as u64
}

/// Writes `frames` of silence (s16le, RATE, CHANNELS).
fn write_silence(out: &mut impl Write, frames: u64) -> std::io::Result<()> {
    let zeros = [0u8; CHUNK_BYTES];
    let mut bytes = frames * FRAME_BYTES as u64;
    while bytes > 0 {
        let n = bytes.min(CHUNK_BYTES as u64) as usize;
        out.write_all(&zeros[..n])?;
        bytes -= n as u64;
    }
    Ok(())
}

/// Maps a linear peak to 0..1 on a -60 dB..0 dB scale.
pub fn to_meter(peak: f32) -> f64 {
    if peak <= 0.0 {
        return 0.0;
    }
    (1.0 - 20.0 * f64::from(peak).log10() / FLOOR_DB).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK_FRAMES: u64 = (CHUNK_BYTES / FRAME_BYTES) as u64;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A timeline that has had a chunk every 20 ms for `secs` seconds.
    fn running(t: Instant, secs: u64) -> Timeline {
        let mut timeline = Timeline::new(t);
        timeline.wrote(frames_in(Duration::from_secs(secs)));
        timeline
    }

    /// The silence written before a chunk that came at `at` after a read that
    /// `waited`, the `first` of a parec, and then the chunk.
    fn write(timeline: &mut Timeline, at: Instant, waited: Duration, first: bool) -> u64 {
        timeline.heard(waited, first);
        let missing = timeline.missing_before(at);
        timeline.wrote(missing + CHUNK_FRAMES);
        missing
    }

    #[test]
    fn audio_that_keeps_coming_needs_nothing() {
        let t = Instant::now();
        let mut timeline = running(t, 10);
        assert_eq!(write(&mut timeline, t + ms(10_020), ms(20), false), 0);
        // Ordinary scheduling: a read a little slower than a chunk.
        assert_eq!(write(&mut timeline, t + ms(10_220), ms(200), false), 0);
        // Late after a slow moment on our side, with the audio waiting in
        // the buffer: it is all still to come.
        assert_eq!(write(&mut timeline, t + ms(10_800), ms(0), false), 0);
    }

    #[test]
    fn a_hole_becomes_silence_once() {
        let t = Instant::now();
        let mut timeline = running(t, 10);
        // A stream held for five seconds, then a chunk.
        assert_eq!(
            write(&mut timeline, t + ms(15_020), ms(5_000), false),
            frames_in(ms(5_000))
        );
        assert_eq!(write(&mut timeline, t + ms(15_040), ms(20), false), 0);
        // A parec that exited and came back a second later.
        assert_eq!(
            write(&mut timeline, t + ms(16_060), ms(100), true),
            frames_in(ms(1_000))
        );
    }

    #[test]
    fn a_hole_across_a_pause_keeps_only_the_recorded_part() {
        let t = Instant::now();
        let mut timeline = running(t, 10);
        // The audio stops at 10 s, pause at 13 s, resume at 20 s, audio at 21 s.
        timeline.set_paused(true, t + ms(13_000));
        timeline.set_paused(false, t + ms(20_000));
        assert_eq!(
            write(&mut timeline, t + ms(21_020), ms(100), true),
            frames_in(ms(4_000))
        );
    }

    #[test]
    fn a_parec_back_during_a_pause_still_fills_the_hole() {
        let t = Instant::now();
        let mut timeline = running(t, 10);
        // The audio stops at 10 s, pause at 10.3 s, the new parec's first
        // chunk at 11 s, not written while paused, resume at 12 s.
        timeline.set_paused(true, t + ms(10_300));
        timeline.heard(ms(100), true);
        timeline.set_paused(false, t + ms(12_000));
        assert_eq!(
            write(&mut timeline, t + ms(12_020), ms(20), false),
            frames_in(ms(300))
        );
    }

    #[test]
    fn a_stall_from_before_the_start_counts_from_the_start() {
        let t = Instant::now();
        // The stream has been held for 8 s; the recording started at t.
        let mut timeline = Timeline::new(t);
        assert_eq!(
            write(&mut timeline, t + ms(3_020), ms(8_000), false),
            frames_in(ms(3_000))
        );
    }

    #[test]
    fn silence_is_zeros_in_whole_frames() {
        let mut out = Vec::new();
        write_silence(&mut out, 1234).unwrap();
        assert_eq!(out.len(), 1234 * FRAME_BYTES);
        assert!(out.iter().all(|&b| b == 0));
    }
}
