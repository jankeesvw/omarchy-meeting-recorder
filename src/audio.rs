//! Audio capture through `parec`: one process per source, kept running for the
//! whole life of the app so the meters work before and after a recording too.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
/// 20 ms of s16le audio.
const CHUNK_BYTES: usize = (RATE / 50 * 2 * CHANNELS) as usize;
/// Three seconds of 20 ms peaks.
pub const HISTORY: usize = 150;
const FLOOR_DB: f64 = -60.0;
/// How much raw audio is kept around for live captions to take a window
/// from, a little more than the window itself so one is always ready.
const RAW_WINDOW_SECS: usize = 6;
const RAW_WINDOW_BYTES: usize = RATE as usize * 2 * CHANNELS as usize * RAW_WINDOW_SECS;

struct Inner {
    levels: VecDeque<f32>,
    file: Option<BufWriter<File>>,
    /// While paused the meters keep running but nothing is written.
    paused: bool,
    /// The last `RAW_WINDOW_SECS` of s16le audio (RATE, CHANNELS), for live
    /// captions to decode a recent window from. Kept regardless of `paused`,
    /// like the levels, so a window is ready the moment captioning resumes.
    raw: VecDeque<u8>,
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
            file: None,
            paused: false,
            raw: VecDeque::with_capacity(RAW_WINDOW_BYTES),
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
        inner.file = Some(file);
        inner.paused = false;
        Ok(())
    }

    pub fn set_paused(&self, paused: bool) {
        self.inner.lock().unwrap().paused = paused;
    }

    pub fn stop_recording(&self) {
        if let Some(mut file) = self.inner.lock().unwrap().file.take() {
            let _ = file.flush();
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

    /// The most recent `secs` seconds of raw audio (s16le, RATE, CHANNELS
    /// interleaved), oldest first. Shorter than asked for until that much
    /// has been captured.
    pub fn recent_raw(&self, secs: u32) -> Vec<u8> {
        let inner = self.inner.lock().unwrap();
        let want = RATE as usize * 2 * CHANNELS as usize * secs as usize;
        let start = inner.raw.len().saturating_sub(want);
        inner.raw.iter().skip(start).copied().collect()
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
    while stdout.read_exact(&mut buf).is_ok() {
        chunks += 1;
        let peak = buf
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs())
            .max()
            .unwrap_or(0) as f32
            / 32768.0;
        let mut inner = shared.lock().unwrap();
        inner.levels.pop_front();
        inner.levels.push_back(peak);
        inner.raw.extend(buf.iter().copied());
        let overflow = inner.raw.len().saturating_sub(RAW_WINDOW_BYTES);
        if overflow > 0 {
            inner.raw.drain(0..overflow);
        }
        if !inner.paused
            && let Some(file) = inner.file.as_mut()
        {
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

/// Maps a linear peak to 0..1 on a -60 dB..0 dB scale.
pub fn to_meter(peak: f32) -> f64 {
    if peak <= 0.0 {
        return 0.0;
    }
    (1.0 - 20.0 * f64::from(peak).log10() / FLOOR_DB).clamp(0.0, 1.0)
}
