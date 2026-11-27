//! Audio capture through `parec`: one process per source, kept running for the
//! whole life of the app so the meters work before and after a recording too.
//!
//! The microphone side can be more than one microphone. Each extra one is
//! recorded into a track of its own next to the first, and when the recording
//! stops they are mixed into it (`mix_extra_tracks`), so everything after that
//! still sees one microphone track.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
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
/// The system's default microphone, whichever it is at the time.
pub const DEFAULT_MIC: &str = "@DEFAULT_SOURCE@";
/// What the computer plays, through the default output's monitor.
pub const COMPUTER_AUDIO: &str = "@DEFAULT_MONITOR@";
const FLOOR_DB: f64 = -60.0;

struct Inner {
    /// What is captured: a PulseAudio source name, "" for nothing.
    device: String,
    levels: VecDeque<f32>,
    recording: Option<Recording>,
    /// While paused the meters keep running but nothing is written.
    paused: bool,
    /// Set by `close`: the capture stops and does not come back.
    closed: bool,
    /// The running `parec`, so `close` can stop it.
    child: Option<u32>,
}

struct Recording {
    file: BufWriter<File>,
    /// Where the recording is, so audio that never came becomes silence.
    timeline: Timeline,
}

#[derive(Clone)]
pub struct Source {
    inner: Arc<Mutex<Inner>>,
    /// More microphones recorded with this one, by device name. Their levels
    /// count in this source's meter, and each records into a track of its own.
    extras: Arc<Mutex<Vec<(String, Source)>>>,
}

impl Source {
    /// Starts capturing `device`, a PulseAudio source name such as `@DEFAULT_MONITOR@`.
    pub fn spawn(device: impl Into<String>) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            device: device.into(),
            levels: VecDeque::from(vec![0.0; HISTORY]),
            recording: None,
            paused: false,
            closed: false,
            child: None,
        }));
        let shared = inner.clone();
        thread::spawn(move || {
            loop {
                let device = {
                    let inner = shared.lock().unwrap();
                    if inner.closed {
                        break;
                    }
                    inner.device.clone()
                };
                if device.is_empty() {
                    thread::sleep(Duration::from_millis(200));
                    continue;
                }
                capture(&device, &shared);
                // parec exits when the device goes away; try again, straight
                // away when it was stopped to capture another one.
                if shared.lock().unwrap().device == device {
                    thread::sleep(Duration::from_secs(1));
                }
            }
        });
        Source {
            inner,
            extras: Arc::default(),
        }
    }

    /// Stops capturing for good.
    fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        inner.recording = None;
        stop_child(&mut inner);
    }

    /// Captures `device` from now on instead of what it captured before.
    fn set_device(&self, device: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.device != device {
            inner.device = device.to_owned();
            stop_child(&mut inner);
        }
    }

    /// Captures `device` again, or nothing (None): then nothing is recorded,
    /// the track stays silent, and the meter lies flat.
    pub fn set_input(&self, device: Option<&str>) {
        self.set_device(device.unwrap_or_default());
        if device.is_none() {
            self.inner
                .lock()
                .unwrap()
                .levels
                .iter_mut()
                .for_each(|l| *l = 0.0);
        }
    }

    /// Records these microphones, by PulseAudio source name: the first in
    /// this source's own track, the others each in a track next to it. None
    /// records no microphone at all. Not to be changed during a recording.
    pub fn set_mics(&self, devices: &[String]) {
        let Some((first, rest)) = devices.split_first() else {
            self.set_input(None);
            self.set_extras(&[]);
            return;
        };
        self.set_input(Some(first));
        let rest: Vec<String> = rest.iter().filter(|d| *d != first).cloned().collect();
        self.set_extras(&rest);
    }

    /// Records these microphones too, in this order. One that is already
    /// captured keeps going; one that is not in the list any more stops.
    fn set_extras(&self, devices: &[String]) {
        let mut extras = self.extras.lock().unwrap();
        let mut kept = Vec::new();
        for device in devices {
            if kept.iter().any(|(d, _): &(String, Source)| d == device) {
                continue;
            }
            let source = match extras.iter().position(|(d, _)| d == device) {
                Some(i) => extras.remove(i).1,
                None => Source::spawn(device.clone()),
            };
            kept.push((device.clone(), source));
        }
        for (_, gone) in extras.drain(..) {
            gone.close();
        }
        *extras = kept;
    }

    /// Tees the raw stream (s16le, RATE, CHANNELS) into `path` from now on,
    /// and every extra microphone into a track of its own next to it
    /// (`mic-2.raw`, `mic-3.raw`, ... beside `mic.raw`).
    pub fn start_recording(&self, path: &Path) -> std::io::Result<()> {
        self.start_own_recording(path)?;
        let extras = self.extras.lock().unwrap();
        for (i, (_, extra)) in extras.iter().enumerate() {
            if let Err(e) = extra.start_own_recording(&extra_track(path, i)) {
                drop(extras);
                self.stop_recording();
                return Err(e);
            }
        }
        Ok(())
    }

    fn start_own_recording(&self, path: &Path) -> std::io::Result<()> {
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
        for source in self.all() {
            let mut inner = source.inner.lock().unwrap();
            inner.paused = paused;
            if let Some(recording) = inner.recording.as_mut() {
                recording.timeline.set_paused(paused, Instant::now());
            }
        }
    }

    pub fn stop_recording(&self) {
        for source in self.all() {
            if let Some(mut recording) = source.inner.lock().unwrap().recording.take() {
                let _ = recording.file.flush();
            }
        }
    }

    /// This source and its extra microphones.
    fn all(&self) -> Vec<Source> {
        let mut all = vec![self.clone()];
        all.extend(self.extras.lock().unwrap().iter().map(|(_, s)| s.clone()));
        all
    }

    /// The peaks of the last three seconds; with extra microphones the
    /// loudest of them at every moment.
    pub fn levels(&self) -> Vec<f32> {
        let mut levels = self.own_levels();
        for (_, extra) in self.extras.lock().unwrap().iter() {
            for (level, other) in levels.iter_mut().zip(extra.own_levels()) {
                *level = level.max(other);
            }
        }
        levels
    }

    fn own_levels(&self) -> Vec<f32> {
        self.inner.lock().unwrap().levels.iter().copied().collect()
    }

    /// The loudest of the last `n` peaks, so a short burst is not missed by a slower reader.
    pub fn recent_peak(&self, n: usize) -> f32 {
        self.all()
            .iter()
            .map(|source| source.own_peak(n))
            .fold(0.0, f32::max)
    }

    fn own_peak(&self, n: usize) -> f32 {
        let inner = self.inner.lock().unwrap();
        inner
            .levels
            .iter()
            .rev()
            .take(n)
            .copied()
            .fold(0.0, f32::max)
    }

    /// The peaks of the last three seconds of one of the microphones
    /// recorded, by the name it was set with; silence for any other.
    pub fn levels_of(&self, device: &str) -> Vec<f32> {
        if self.inner.lock().unwrap().device == device {
            return self.own_levels();
        }
        let extras = self.extras.lock().unwrap();
        match extras.iter().find(|(d, _)| d == device) {
            Some((_, source)) => source.own_levels(),
            None => vec![0.0; HISTORY],
        }
    }
}

/// Stops the running `parec`, if there is one; the capture loop then moves on.
fn stop_child(inner: &mut Inner) {
    if let Some(pid) = inner.child.take() {
        // SAFETY: kill only sends a signal, to the parec this source started.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

/// Where the `i`th extra microphone records, next to the first one's `path`:
/// `mic-2.raw` for the first extra beside `mic.raw`.
fn extra_track(path: &Path, i: usize) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("mic");
    path.with_file_name(format!("{stem}-{}.raw", i + 2))
}

/// The extra microphones' tracks recorded next to `path`, in order.
fn extra_tracks(path: &Path) -> Vec<PathBuf> {
    (0..)
        .map(|i| extra_track(path, i))
        .take_while(|p| p.is_file())
        .collect()
}

/// A microphone the system has: its PulseAudio source name and what it is called.
#[derive(Clone, Debug, PartialEq)]
pub struct Microphone {
    pub name: String,
    pub label: String,
}

/// The microphones there are now, and the name of the default one. Monitors
/// (what an output plays) are not microphones and are left out.
pub fn microphones() -> (Vec<Microphone>, String) {
    let listed = Command::new("pactl")
        .args(["--format=json", "list", "sources"])
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let default = Command::new("pactl")
        .arg("get-default-source")
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    (parse_microphones(&listed), default)
}

/// The microphones to record, in the order of `present`: the ones `chosen`
/// that are plugged in, or the default one when none of those is, or when
/// nothing was ever chosen (None). Chosen to be none at all, none it is.
pub fn pick_mics(present: &[Microphone], default: &str, chosen: Option<&[String]>) -> Vec<String> {
    if chosen.is_some_and(|c| c.is_empty()) {
        return Vec::new();
    }
    let chosen = chosen.unwrap_or_default();
    let picked: Vec<String> = present
        .iter()
        .filter(|m| chosen.contains(&m.name))
        .map(|m| m.name.clone())
        .collect();
    if !picked.is_empty() {
        return picked;
    }
    present
        .iter()
        .find(|m| m.name == default)
        .or(present.first())
        .map(|m| vec![m.name.clone()])
        .unwrap_or_default()
}

fn parse_microphones(json: &str) -> Vec<Microphone> {
    let Ok(serde_json::Value::Array(sources)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    sources
        .iter()
        .filter_map(|source| {
            let name = source["name"].as_str()?;
            let properties = &source["properties"];
            let monitor = properties["device.class"].as_str() == Some("monitor")
                || name.ends_with(".monitor");
            let input = properties["media.class"]
                .as_str()
                .is_none_or(|class| class.starts_with("Audio/Source"));
            if monitor || !input {
                return None;
            }
            let label = source["description"]
                .as_str()
                .filter(|d| !d.trim().is_empty())
                .unwrap_or(name);
            Some(Microphone {
                name: name.to_owned(),
                label: label.to_owned(),
            })
        })
        .collect()
}

/// Mixes the extra microphones' tracks recorded next to `path` into it, and
/// removes them. Nothing to do without extra microphones.
///
/// Two microphones in one room both hear everyone, a little later in the
/// farther one, and simply adding them makes every voice hollow. So the mix
/// shares the gain between them the way a conference automixer does (Dugan's
/// gain sharing): moment by moment each microphone gets its share of the
/// total sound, the person speaking is heard through their own microphone,
/// and together the gains always add up to one, so the level stays the same
/// as with one microphone and the room noise does not add up.
pub fn mix_extra_tracks(path: &Path) -> std::io::Result<()> {
    let extras = extra_tracks(path);
    if extras.is_empty() {
        return Ok(());
    }
    let mixed = path.with_extension("mixed");
    {
        let mut inputs: Vec<Box<dyn Read>> = Vec::new();
        // No first track (no microphone at all when it started) is silence.
        inputs.push(match File::open(path) {
            Ok(file) => Box::new(BufReader::new(file)),
            Err(_) => Box::new(std::io::empty()),
        });
        for extra in &extras {
            inputs.push(Box::new(BufReader::new(File::open(extra)?)));
        }
        let mut out = BufWriter::new(File::create(&mixed)?);
        gain_share(&mut inputs, &mut out)?;
        out.flush()?;
        out.get_ref().sync_data()?;
    }
    std::fs::rename(&mixed, path)?;
    for extra in extras {
        let _ = std::fs::remove_file(extra);
    }
    Ok(())
}

/// Gain sharing over tracks of s16le, RATE, CHANNELS, read until the longest
/// ends; a shorter one is silence after its end.
fn gain_share(inputs: &mut [Box<dyn Read>], out: &mut impl Write) -> std::io::Result<()> {
    /// How much of the last block's level carries over: about 50 ms of memory,
    /// quick enough to follow who speaks, slow enough not to flutter.
    const SMOOTHING: f64 = 0.6;
    let samples = CHUNK_BYTES / 2;
    let n = inputs.len();
    let mut blocks = vec![vec![0f32; samples]; n];
    let mut raw = vec![0u8; CHUNK_BYTES];
    let mut envelope = vec![0f64; n];
    let mut gains = vec![1.0 / n as f64; n];
    let mut bytes = vec![0u8; CHUNK_BYTES];
    loop {
        let mut any = false;
        for (input, block) in inputs.iter_mut().zip(blocks.iter_mut()) {
            let got = read_full(input, &mut raw)?;
            any |= got > 0;
            raw[got..].fill(0);
            for (sample, b) in block.iter_mut().zip(raw.as_chunks::<2>().0) {
                *sample = f32::from(i16::from_le_bytes(*b)) / 32768.0;
            }
        }
        if !any {
            return Ok(());
        }
        for (e, block) in envelope.iter_mut().zip(&blocks) {
            let power = block.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / samples as f64;
            *e = SMOOTHING * *e + (1.0 - SMOOTHING) * power;
        }
        let total: f64 = envelope.iter().sum::<f64>() + 1e-12;
        let target: Vec<f64> = envelope.iter().map(|e| e / total).collect();
        // From the last block's gains to this one's across the block, so the
        // change is never a step you can hear.
        let frames = samples / CHANNELS as usize;
        for i in 0..samples {
            let t = (i / CHANNELS as usize) as f64 / frames as f64;
            let mut sum = 0.0;
            for k in 0..n {
                let gain = gains[k] + (target[k] - gains[k]) * t;
                sum += f64::from(blocks[k][i]) * gain;
            }
            let value = (sum * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
            bytes[i * 2..i * 2 + 2].copy_from_slice(&value.to_le_bytes());
        }
        gains = target;
        out.write_all(&bytes)?;
    }
}

/// Reads until `buf` is full or the input ends; the number of bytes read.
fn read_full(input: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match input.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

fn capture(device: &str, shared: &Mutex<Inner>) {
    let mut command = Command::new("parec");
    command
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
        .stderr(Stdio::null());
    // parec stops with the app, even after a crash. Left on its own it keeps
    // running once nobody reads it any more, at times at a full core (#55).
    // SAFETY: prctl is async-signal-safe and touches only the child's own state.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let Ok(mut child) = command.spawn() else {
        return;
    };
    {
        let mut inner = shared.lock().unwrap();
        // Closed, or set to another device, while this one was starting.
        if inner.closed || inner.device != device {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        // Until it is waited for below the pid stays this child's, even after it exits.
        inner.child = Some(child.id());
    }
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
    shared.lock().unwrap().child = None;
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

    /// `secs` of a sine at `hz` and `amplitude`, s16le in both channels.
    fn tone(hz: f64, amplitude: f64, secs: f64) -> Vec<u8> {
        let frames = (secs * f64::from(RATE)) as usize;
        let mut out = Vec::with_capacity(frames * FRAME_BYTES);
        for i in 0..frames {
            let v = (amplitude
                * (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(RATE)).sin()
                * 32767.0) as i16;
            for _ in 0..CHANNELS {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        out
    }

    fn rms(bytes: &[u8]) -> f64 {
        let samples: Vec<f64> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| f64::from(i16::from_le_bytes(*b)) / 32768.0)
            .collect();
        (samples.iter().map(|s| s * s).sum::<f64>() / samples.len().max(1) as f64).sqrt()
    }

    fn share(tracks: Vec<Vec<u8>>) -> Vec<u8> {
        let mut inputs: Vec<Box<dyn Read>> = tracks
            .into_iter()
            .map(|t| Box::new(std::io::Cursor::new(t)) as Box<dyn Read>)
            .collect();
        let mut out = Vec::new();
        gain_share(&mut inputs, &mut out).unwrap();
        out
    }

    #[test]
    fn the_one_speaking_keeps_their_level() {
        // One mic hears its speaker, the other only a faint copy (-20 dB).
        let near = tone(220.0, 0.3, 1.0);
        let far = tone(220.0, 0.03, 1.0);
        let mixed = share(vec![near.clone(), far]);
        assert_eq!(mixed.len(), near.len());
        // After the first 100 ms the near mic has nearly all the gain.
        let settle = (RATE as usize / 10) * FRAME_BYTES;
        let ratio = rms(&mixed[settle..]) / rms(&near[settle..]);
        assert!((0.95..1.05).contains(&ratio), "ratio {ratio}");
    }

    #[test]
    fn a_silent_mic_takes_nothing_away() {
        let voice = tone(300.0, 0.25, 1.0);
        let silent = vec![0u8; voice.len()];
        let mixed = share(vec![silent, voice.clone()]);
        let settle = (RATE as usize / 10) * FRAME_BYTES;
        let ratio = rms(&mixed[settle..]) / rms(&voice[settle..]);
        assert!((0.98..1.02).contains(&ratio), "ratio {ratio}");
    }

    #[test]
    fn the_same_sound_twice_is_not_louder() {
        let voice = tone(150.0, 0.4, 0.5);
        let mixed = share(vec![voice.clone(), voice.clone()]);
        let ratio = rms(&mixed) / rms(&voice);
        assert!((0.97..1.03).contains(&ratio), "ratio {ratio}");
    }

    #[test]
    fn a_shorter_track_is_silence_after_its_end() {
        let long = tone(200.0, 0.2, 1.0);
        let short = tone(200.0, 0.2, 0.3);
        let mixed = share(vec![short, long.clone()]);
        assert_eq!(mixed.len(), long.len());
        let tail = (RATE as usize / 2) * FRAME_BYTES;
        let ratio = rms(&mixed[tail..]) / rms(&long[tail..]);
        assert!((0.98..1.02).contains(&ratio), "ratio {ratio}");
    }

    #[test]
    fn extra_tracks_are_mixed_in_and_removed() {
        let dir = std::env::temp_dir().join(format!("mr-mix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mic = dir.join("mic.raw");
        assert_eq!(extra_track(&mic, 0), dir.join("mic-2.raw"));
        assert_eq!(extra_track(&mic, 1), dir.join("mic-3.raw"));
        // Without extra tracks the first one is left alone.
        std::fs::write(&mic, tone(200.0, 0.2, 0.2)).unwrap();
        mix_extra_tracks(&mic).unwrap();
        assert_eq!(std::fs::read(&mic).unwrap(), tone(200.0, 0.2, 0.2));
        std::fs::write(dir.join("mic-2.raw"), tone(400.0, 0.2, 0.5)).unwrap();
        std::fs::write(dir.join("mic-3.raw"), []).unwrap();
        mix_extra_tracks(&mic).unwrap();
        assert_eq!(
            std::fs::metadata(&mic).unwrap().len() as usize,
            tone(400.0, 0.2, 0.5).len()
        );
        assert!(!dir.join("mic-2.raw").exists() && !dir.join("mic-3.raw").exists());
        assert!(!dir.join("mic.mixed").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_chosen_microphones_that_are_there_are_recorded() {
        let mic = |name: &str| Microphone {
            name: name.to_owned(),
            label: name.to_uppercase(),
        };
        let present = [mic("rode"), mic("dji"), mic("webcam")];
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let pick = |default: &str, chosen: Option<&[&str]>| {
            let chosen = chosen.map(names);
            pick_mics(&present, default, chosen.as_deref())
        };
        // Nothing chosen yet: the default one.
        assert_eq!(pick("dji", None), names(&["dji"]));
        // Chosen ones in the order of the list, the ones not plugged in left out.
        assert_eq!(
            pick("rode", Some(&["webcam", "gone", "rode"])),
            names(&["rode", "webcam"])
        );
        assert_eq!(pick("rode", Some(&["dji"])), names(&["dji"]));
        // None of the chosen ones there: the default, else the first.
        assert_eq!(pick("rode", Some(&["gone"])), names(&["rode"]));
        assert_eq!(pick("", Some(&["gone"])), names(&["rode"]));
        // No microphone at all, on purpose: only the computer audio.
        assert!(pick("rode", Some(&[])).is_empty());
        assert!(pick_mics(&[], "rode", None).is_empty());
    }

    #[test]
    fn microphones_leave_out_monitors() {
        let json = r#"[
            {"name": "alsa_input.usb-RODE-00.mono-fallback", "description": "Podcaster Mono",
             "properties": {"device.class": "sound", "media.class": "Audio/Source"}},
            {"name": "alsa_output.pci.analog-stereo.monitor", "description": "Monitor of Headphone",
             "properties": {"device.class": "monitor", "media.class": "Audio/Sink"}},
            {"name": "alsa_input.usb-DJI-01.analog-stereo", "description": "",
             "properties": {"device.class": "sound", "media.class": "Audio/Source"}},
            {"name": "rnnoise_source", "description": "Noise canceled mic",
             "properties": {"media.class": "Audio/Source/Virtual"}}
        ]"#;
        let found = parse_microphones(json);
        let names: Vec<(&str, &str)> = found
            .iter()
            .map(|m| (m.name.as_str(), m.label.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("alsa_input.usb-RODE-00.mono-fallback", "Podcaster Mono"),
                (
                    "alsa_input.usb-DJI-01.analog-stereo",
                    "alsa_input.usb-DJI-01.analog-stereo"
                ),
                ("rnnoise_source", "Noise canceled mic"),
            ]
        );
        assert!(parse_microphones("not json").is_empty());
    }
}
