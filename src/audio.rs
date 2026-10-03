//! Audio capture through `parec`: one process per source, kept running for the
//! whole life of the app so the meters work before and after a recording too.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
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

/// The system default microphone, as `parec` names it.
pub const DEFAULT_MIC: &str = "@DEFAULT_SOURCE@";
/// The monitor of the system default output, as `parec` names it.
pub const DEFAULT_SYSTEM: &str = "@DEFAULT_MONITOR@";

/// Which side of the call a source records.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// You, through a microphone.
    Mic,
    /// The computer audio, through the monitor of an output.
    System,
}

impl Role {
    /// The system default for this side, as `parec` names it.
    pub fn default_device(self) -> &'static str {
        match self {
            Role::Mic => DEFAULT_MIC,
            Role::System => DEFAULT_SYSTEM,
        }
    }
}

struct Inner {
    levels: VecDeque<f32>,
    recording: Option<Recording>,
    /// While paused the meters keep running but nothing is written.
    paused: bool,
    /// The PulseAudio source being captured.
    device: String,
    /// The running `parec`, so a new device can take its place.
    child: Option<Child>,
    /// Set when the device changed, so the capture restarts at once.
    switched: bool,
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
    pub fn spawn(device: String) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            levels: VecDeque::from(vec![0.0; HISTORY]),
            recording: None,
            paused: false,
            device,
            child: None,
            switched: false,
        }));
        let shared = inner.clone();
        thread::spawn(move || {
            loop {
                capture(&shared);
                // parec exits when the device goes away; try again. After a
                // switch the new device starts right away, so a recording
                // loses as little as possible.
                if !std::mem::take(&mut shared.lock().unwrap().switched) {
                    thread::sleep(Duration::from_secs(1));
                }
            }
        });
        Source { inner }
    }

    pub fn device(&self) -> String {
        self.inner.lock().unwrap().device.clone()
    }

    /// Captures `device` from now on; a recording carries on into the same file.
    pub fn set_device(&self, device: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.device == device {
            return;
        }
        inner.device = device.to_owned();
        inner.switched = true;
        if let Some(child) = inner.child.as_mut() {
            let _ = child.kill();
        }
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

fn is_default(device: &str) -> bool {
    device == DEFAULT_MIC || device == DEFAULT_SYSTEM
}

fn capture(shared: &Mutex<Inner>) {
    let device = shared.lock().unwrap().device.clone();
    let mut command = Command::new("parec");
    command.args([
        "--raw",
        "--format=s16le",
        &format!("--rate={RATE}"),
        &format!("--channels={CHANNELS}"),
        "--latency-msec=20",
        "-d",
        &device,
    ]);
    if !is_default(&device) {
        // Otherwise PipeWire quietly records the default device in its place
        // while it is unplugged or not there yet.
        command.arg("--property=node.dont-fallback=true");
    }
    let Ok(mut child) = command.stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else {
        return;
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    {
        let mut inner = shared.lock().unwrap();
        // Switched while this one was starting: let it end and start the new one.
        if inner.device != device {
            let _ = child.kill();
        }
        inner.child = Some(child);
    }
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
    let child = shared.lock().unwrap().child.take();
    if let Some(mut child) = child {
        let _ = child.kill();
        let _ = child.wait();
    }
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

/// A microphone or an output to record from: its PulseAudio source name and
/// what to call it in the menu.
#[derive(Clone, PartialEq, Debug)]
pub struct Device {
    pub name: String,
    pub label: String,
}

/// The microphones and the outputs (through their monitors) there are now,
/// and what the system defaults are called.
#[derive(Clone, Default, PartialEq)]
pub struct Devices {
    pub mics: Vec<Device>,
    pub outputs: Vec<Device>,
    pub default_mic: Option<String>,
    pub default_output: Option<String>,
}

impl Devices {
    /// The devices for `role`, and what its system default is called.
    pub fn of(&self, role: Role) -> (&[Device], Option<&str>) {
        match role {
            Role::Mic => (&self.mics, self.default_mic.as_deref()),
            Role::System => (&self.outputs, self.default_output.as_deref()),
        }
    }
}

pub fn devices() -> Devices {
    let pactl = |args: &[&str]| {
        Command::new("pactl")
            .args(["-f", "json"])
            .args(args)
            .stderr(Stdio::null())
            .output()
            .map(|out| out.stdout)
            .unwrap_or_default()
    };
    parse_devices(
        &pactl(&["list", "sources"]),
        &pactl(&["list", "sinks"]),
        &pactl(&["info"]),
    )
}

/// The devices in `pactl -f json list sources`, `... list sinks` and
/// `... info`. An output is called by its own name, as the name of its
/// monitor is "Monitor of ..." in the language of the sound server.
fn parse_devices(sources: &[u8], sinks: &[u8], info: &[u8]) -> Devices {
    let sources: Vec<serde_json::Value> = serde_json::from_slice(sources).unwrap_or_default();
    let sinks: Vec<serde_json::Value> = serde_json::from_slice(sinks).unwrap_or_default();
    let info: serde_json::Value = serde_json::from_slice(info).unwrap_or_default();
    let mut devices = Devices::default();
    for source in &sources {
        let (Some(name), Some(label)) = (source["name"].as_str(), source["description"].as_str())
        else {
            continue;
        };
        let sink = sinks
            .iter()
            .find(|sink| sink["monitor_source"].as_str() == Some(name));
        if sink.is_some() || source["properties"]["device.class"].as_str() == Some("monitor") {
            let label = sink
                .and_then(|sink| sink["description"].as_str())
                .or_else(|| label.strip_prefix("Monitor of "))
                .unwrap_or(label);
            devices.outputs.push(Device {
                name: name.to_owned(),
                label: label.to_owned(),
            });
        } else {
            if info["default_source_name"].as_str() == Some(name) {
                devices.default_mic = Some(label.to_owned());
            }
            devices.mics.push(Device {
                name: name.to_owned(),
                label: label.to_owned(),
            });
        }
    }
    devices.default_output = sinks
        .iter()
        .find(|sink| sink["name"] == info["default_sink_name"])
        .and_then(|sink| sink["description"].as_str())
        .map(str::to_owned);
    devices
}

/// Calls `changed` whenever a microphone or an output comes, goes or
/// changes, or another one becomes the default, until it returns false.
pub fn watch_devices(changed: impl Fn() -> bool + Send + 'static) {
    thread::spawn(move || {
        loop {
            let Ok(mut child) = Command::new("pactl")
                .arg("subscribe")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            else {
                thread::sleep(Duration::from_secs(5));
                continue;
            };
            let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
            let mut wanted = true;
            for line in stdout.lines() {
                let Ok(line) = line else {
                    break;
                };
                // "Event 'new' on source #63", "Event 'change' on server #-1"
                if [" on source ", " on sink ", " on server "]
                    .iter()
                    .any(|on| line.contains(on))
                    && !changed()
                {
                    wanted = false;
                    break;
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            if !wanted {
                return;
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
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

    // Cut down from what `pactl -f json list sources` and `... list sinks`
    // print on PipeWire, the monitor in German.
    const SOURCES: &str = r#"[
        {"name": "alsa_output.usb-Sennheiser_BTD_800.analog-stereo.monitor",
         "description": "Monitor von BTD-800 Analog Stereo",
         "properties": {"device.class": "monitor"}},
        {"name": "alsa_input.usb-Sennheiser_BTD_800.mono-fallback",
         "description": "BTD-800 Mono",
         "properties": {"device.class": "sound"}},
        {"name": "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor",
         "description": "Monitor of Built-in Audio Analog Stereo",
         "properties": {"device.class": "monitor"}},
        {"name": "no_description"}
    ]"#;
    const SINKS: &str = r#"[
        {"name": "alsa_output.usb-Sennheiser_BTD_800.analog-stereo",
         "description": "BTD-800 Analog Stereo",
         "monitor_source": "alsa_output.usb-Sennheiser_BTD_800.analog-stereo.monitor"}
    ]"#;

    const INFO: &str = r#"{
        "default_sink_name": "alsa_output.usb-Sennheiser_BTD_800.analog-stereo",
        "default_source_name": "alsa_input.usb-Sennheiser_BTD_800.mono-fallback"
    }"#;

    #[test]
    fn devices_split_into_mics_and_outputs() {
        let Devices {
            mics,
            outputs,
            default_mic,
            default_output,
        } = parse_devices(SOURCES.as_bytes(), SINKS.as_bytes(), INFO.as_bytes());
        let labels = |devices: &[Device]| -> Vec<String> {
            devices.iter().map(|d| d.label.clone()).collect()
        };
        assert_eq!(labels(&mics), ["BTD-800 Mono"]);
        assert_eq!(
            mics[0].name,
            "alsa_input.usb-Sennheiser_BTD_800.mono-fallback"
        );
        // Named after the sink, whatever the language; without one the
        // English prefix is still dropped.
        assert_eq!(
            labels(&outputs),
            ["BTD-800 Analog Stereo", "Built-in Audio Analog Stereo"]
        );
        assert_eq!(
            outputs[0].name,
            "alsa_output.usb-Sennheiser_BTD_800.analog-stereo.monitor"
        );
        assert_eq!(default_mic.as_deref(), Some("BTD-800 Mono"));
        assert_eq!(default_output.as_deref(), Some("BTD-800 Analog Stereo"));
    }

    #[test]
    fn devices_from_nothing() {
        assert!(parse_devices(b"", b"", b"") == Devices::default());
        let devices = parse_devices(SOURCES.as_bytes(), b"not json", b"");
        assert_eq!((devices.mics.len(), devices.outputs.len()), (1, 2));
        assert_eq!((devices.default_mic, devices.default_output), (None, None));
    }

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
