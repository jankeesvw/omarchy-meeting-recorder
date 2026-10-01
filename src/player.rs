//! The player on the done page: a two-lane waveform of the meeting (you above
//! the line, the other side below it) with a playhead, click or drag to seek.
//!
//! Playback is `ffmpeg` decoding into `pacat`, not GStreamer: a stock Omarchy
//! has no GStreamer audio sink, while `pacat` comes with the same package as
//! the `parec` the recorder already records with. Pausing stops the pipeline
//! and playing starts it again at the position; a meeting saved as separate
//! files is mixed on the fly.

use std::cell::{Cell, RefCell};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gio, glib};

use crate::export;

const BINS: usize = 1000;
const MIC_COLOR: (f64, f64, f64) = (0.21, 0.52, 0.89);
const SYSTEM_COLOR: (f64, f64, f64) = (0.90, 0.38, 0.0);

type PositionCallback = Rc<RefCell<Option<Box<dyn Fn(i64)>>>>;

/// A running `ffmpeg | pacat` pipeline, stopped when dropped.
struct Playback {
    ffmpeg: Child,
    pacat: Child,
    started: Instant,
    from_us: i64,
}

impl Playback {
    fn start(files: &[PathBuf], from_us: i64) -> Option<Playback> {
        let at = format!("{:.3}", from_us as f64 / 1_000_000.0);
        let mut ffmpeg = Command::new("ffmpeg");
        ffmpeg.args(["-v", "error", "-nostdin"]);
        for file in files {
            ffmpeg.args(["-ss", &at, "-i"]).arg(file);
        }
        if files.len() > 1 {
            ffmpeg.args([
                "-filter_complex",
                &format!("amix=inputs={}:normalize=0", files.len()),
            ]);
        }
        ffmpeg
            .args(["-f", "s16le", "-ar", "48000", "-ac", "2", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut ffmpeg = die_with_parent(&mut ffmpeg).spawn().ok()?;
        let audio = ffmpeg.stdout.take()?;
        let pacat = die_with_parent(
            Command::new("pacat")
                .args([
                    "--playback",
                    "--raw",
                    "--format=s16le",
                    "--rate=48000",
                    "--channels=2",
                    "--latency-msec=80",
                    "--client-name=Meeting Recorder",
                ])
                .stdin(Stdio::from(audio))
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .spawn();
        match pacat {
            Ok(pacat) => Some(Playback {
                ffmpeg,
                pacat,
                started: Instant::now(),
                from_us,
            }),
            Err(_) => {
                let _ = ffmpeg.kill();
                let _ = ffmpeg.wait();
                None
            }
        }
    }

    fn position_us(&self) -> i64 {
        self.from_us + self.started.elapsed().as_micros() as i64
    }

    fn ended(&mut self) -> bool {
        matches!(self.pacat.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        let _ = self.ffmpeg.kill();
        let _ = self.pacat.kill();
        let _ = self.ffmpeg.wait();
        let _ = self.pacat.wait();
    }
}

/// Makes the child get SIGTERM when the app goes away, even after a crash, so
/// the meeting never keeps playing on its own.
fn die_with_parent(command: &mut Command) -> &mut Command {
    // SAFETY: prctl is async-signal-safe and touches only the child's own state.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        })
    }
}

/// Length of an audio file in microseconds, from ffprobe.
fn probe_duration_us(path: &Path) -> i64 {
    Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.trim().parse::<f64>().ok())
        .map(|secs| (secs * 1_000_000.0) as i64)
        .unwrap_or(0)
}

#[derive(Default)]
struct State {
    files: Vec<PathBuf>,
    duration_us: i64,
    /// Where playback starts from next, while paused.
    paused_at_us: i64,
    playback: Option<Playback>,
    /// Peaks per bin for the mic and the computer track, 0..1.
    peaks: Option<(Vec<f32>, Vec<f32>)>,
    /// Bumped on every load, so a slow waveform for an old meeting is dropped.
    generation: u64,
    /// Chapter starts in ms with their titles, drawn as markers.
    chapters: Vec<(i64, String)>,
}

impl State {
    fn relocate(&mut self, from: &Path, to: &Path) {
        for path in &mut self.files {
            if let Ok(relative) = path.strip_prefix(from) {
                *path = to.join(relative);
            }
        }
    }
}

#[derive(Clone)]
pub struct Player {
    root: gtk::Box,
    button: gtk::Button,
    wave: gtk::DrawingArea,
    time: gtk::Label,
    state: Rc<RefCell<State>>,
    /// Called with the position in ms while playing, for the transcript highlight.
    on_position: PositionCallback,
    ticking: Rc<Cell<bool>>,
}

impl Player {
    pub fn new() -> Self {
        let button = gtk::Button::builder()
            .icon_name("media-playback-start-symbolic")
            .tooltip_text("Play")
            .valign(gtk::Align::Center)
            .css_classes(["circular", "flat"])
            .build();
        let wave = gtk::DrawingArea::builder()
            .content_height(56)
            .hexpand(true)
            .build();
        wave.set_cursor_from_name(Some("pointer"));
        let time = gtk::Label::builder()
            .label("00:00")
            .css_classes(["numeric", "caption", "dim-label"])
            .valign(gtk::Align::Center)
            .build();
        let root = gtk::Box::builder()
            .spacing(10)
            .css_classes(["card", "player"])
            .build();

        root.append(&button);
        root.append(&wave);
        root.append(&time);

        let player = Player {
            root,
            button,
            wave,
            time,
            state: Rc::default(),
            on_position: Rc::default(),
            ticking: Rc::default(),
        };

        let this = player.clone();
        player.wave.set_draw_func(move |_, cr, width, height| {
            this.draw(cr, f64::from(width), f64::from(height));
        });

        let this = player.clone();
        player.button.connect_clicked(move |_| this.toggle());

        // Click or drag anywhere on the waveform to seek.
        let drag = gtk::GestureDrag::new();
        let this = player.clone();
        drag.connect_drag_begin(move |_, x, _| this.seek_to_x(x));
        let this = player.clone();
        drag.connect_drag_update(move |gesture, dx, _| {
            if let Some((x, _)) = gesture.start_point() {
                this.seek_to_x(x + dx);
            }
        });
        player.wave.add_controller(drag);

        // Hovering a chapter marker shows its title.
        player.wave.set_has_tooltip(true);
        let this = player.clone();
        player
            .wave
            .connect_query_tooltip(move |_, x, _, _, tooltip| {
                match this.chapter_near(f64::from(x)) {
                    Some(title) => {
                        tooltip.set_text(Some(&title));
                        true
                    }
                    None => false,
                }
            });

        player
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn connect_position(&self, callback: impl Fn(i64) + 'static) {
        *self.on_position.borrow_mut() = Some(Box::new(callback));
    }

    /// Loads the meeting in `dir`: its audio for playback, its tracks for the waveform.
    pub fn load(&self, dir: &Path) {
        self.unload();
        let playable: Vec<PathBuf> = if dir.join("audio.ogg").is_file() {
            vec![dir.join("audio.ogg")]
        } else {
            ["mic.ogg", "computer.ogg"]
                .iter()
                .map(|f| dir.join(f))
                .filter(|p| p.is_file())
                .collect()
        };
        let generation = {
            let mut state = self.state.borrow_mut();
            state.duration_us = playable
                .iter()
                .map(|p| probe_duration_us(p))
                .max()
                .unwrap_or(0);
            state.files = playable.clone();
            state.paused_at_us = 0;
            state.generation += 1;
            state.generation
        };
        self.root.set_visible(!playable.is_empty());

        // The kept tracks give each side its own lane; without them, one lane.
        let (mic, computer) = export::tracks(dir);
        let sources = if mic.is_file() && computer.is_file() {
            (mic, Some(computer))
        } else if let Some(first) = playable.first() {
            (first.clone(), None)
        } else {
            return;
        };
        let this = self.clone();
        glib::spawn_future_local(async move {
            let peaks = gio::spawn_blocking(move || {
                let mic = peaks(&sources.0);
                let computer = sources.1.as_deref().map(peaks);
                (mic, computer)
            })
            .await;
            if let Ok((Some(mic), computer)) = peaks {
                let mut state = this.state.borrow_mut();
                if state.generation == generation {
                    let computer = computer.flatten().unwrap_or_else(|| vec![0.0; BINS]);
                    state.peaks = Some((mic, computer));
                }
            }
            this.wave.queue_draw();
        });
        self.refresh();
    }

    /// Follow a folder rename without resetting position, chapters or playback.
    /// Existing decoder processes retain their open files across the rename.
    pub fn relocate(&self, from: &Path, to: &Path) {
        self.state.borrow_mut().relocate(from, to);
    }

    /// Shows chapter markers on the waveform; empty removes them.
    pub fn set_chapters(&self, chapters: Vec<(i64, String)>) {
        self.state.borrow_mut().chapters = chapters;
        self.wave.queue_draw();
    }

    fn chapter_near(&self, x: f64) -> Option<String> {
        let duration = self.duration_us();
        if duration <= 0 {
            return None;
        }
        let width = f64::from(self.wave.width()).max(1.0);
        self.state
            .borrow()
            .chapters
            .iter()
            .map(|(ms, title)| ((*ms * 1000) as f64 / duration as f64 * width, title))
            .filter(|(at, _)| (at - x).abs() <= 6.0)
            .min_by(|a, b| (a.0 - x).abs().total_cmp(&(b.0 - x).abs()))
            .map(|(_, title)| title.clone())
    }

    /// Stops playback and forgets the meeting.
    pub fn unload(&self) {
        let mut state = self.state.borrow_mut();
        state.playback = None;
        state.files.clear();
        state.duration_us = 0;
        state.paused_at_us = 0;
        state.peaks = None;
        state.chapters.clear();
        drop(state);
        self.set_playing_icon(false);
        self.wave.queue_draw();
    }

    fn duration_us(&self) -> i64 {
        self.state.borrow().duration_us
    }

    fn position_us(&self) -> i64 {
        let state = self.state.borrow();
        match &state.playback {
            Some(playback) => playback.position_us().min(state.duration_us),
            None => state.paused_at_us,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.state.borrow().playback.is_some()
    }

    fn toggle(&self) {
        if self.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    pub fn play(&self) {
        let started = {
            let mut state = self.state.borrow_mut();
            if state.files.is_empty() {
                return;
            }
            if state.paused_at_us >= state.duration_us {
                state.paused_at_us = 0;
            }
            state.playback = None;
            state.playback = Playback::start(&state.files, state.paused_at_us);
            state.playback.is_some()
        };
        self.set_playing_icon(started);
        if started {
            self.start_ticking();
        }
    }

    pub fn pause(&self) {
        let position = self.position_us();
        {
            let mut state = self.state.borrow_mut();
            state.playback = None;
            state.paused_at_us = position;
        }
        self.set_playing_icon(false);
        self.refresh();
    }

    /// Jumps to `ms` and plays from there, for a click on the transcript.
    pub fn play_from(&self, ms: i64) {
        self.state.borrow_mut().paused_at_us = (ms * 1000).max(0);
        self.play();
    }

    fn seek(&self, us: i64) {
        let us = us.clamp(0, self.duration_us().max(0));
        let playing = self.is_playing();
        self.state.borrow_mut().paused_at_us = us;
        if playing {
            self.play();
        }
        self.refresh();
    }

    fn seek_to_x(&self, x: f64) {
        let width = f64::from(self.wave.width()).max(1.0);
        let fraction = (x / width).clamp(0.0, 1.0);
        self.seek((self.duration_us() as f64 * fraction) as i64);
    }

    fn set_playing_icon(&self, playing: bool) {
        self.button.set_icon_name(if playing {
            "media-playback-pause-symbolic"
        } else {
            "media-playback-start-symbolic"
        });
        self.button
            .set_tooltip_text(Some(if playing { "Pause" } else { "Play" }));
    }

    fn start_ticking(&self) {
        if self.ticking.replace(true) {
            return;
        }
        let this = self.clone();
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let ended = this
                .state
                .borrow_mut()
                .playback
                .as_mut()
                .is_some_and(Playback::ended);
            if ended {
                this.pause();
                this.state.borrow_mut().paused_at_us = 0;
            }
            this.refresh();
            if this.is_playing() {
                glib::ControlFlow::Continue
            } else {
                this.ticking.set(false);
                glib::ControlFlow::Break
            }
        });
    }

    fn refresh(&self) {
        let (position, duration) = (self.position_us(), self.duration_us());
        self.time.set_label(&format!(
            "{} / {}",
            clock(position / 1_000_000),
            clock(duration / 1_000_000)
        ));
        self.wave.queue_draw();
        if let Some(callback) = self.on_position.borrow().as_ref() {
            callback(position / 1000);
        }
    }

    fn draw(&self, cr: &gtk::cairo::Context, width: f64, height: f64) {
        let state = self.state.borrow();
        let duration = self.duration_us();
        let played = if duration > 0 {
            self.position_us() as f64 / duration as f64
        } else {
            0.0
        };
        let mid = height / 2.0;
        let step = width / BINS as f64;
        let bar = (step * 0.8).max(1.0);
        let head = played * width;

        match state.peaks.as_ref() {
            Some((mic, computer)) => {
                for i in 0..BINS {
                    let x = i as f64 * step;
                    let alpha = if x <= head { 1.0 } else { 0.38 };
                    let up = (f64::from(mic[i]) * (mid - 2.0)).max(0.5);
                    let down = (f64::from(computer[i]) * (mid - 2.0)).max(0.5);
                    let (r, g, b) = crate::theme::color("blue", MIC_COLOR);
                    cr.set_source_rgba(r, g, b, alpha);
                    cr.rectangle(x, mid - up, bar, up);
                    let _ = cr.fill();
                    let (r, g, b) = crate::theme::color("orange", SYSTEM_COLOR);
                    cr.set_source_rgba(r, g, b, alpha);
                    cr.rectangle(x, mid, bar, down);
                    let _ = cr.fill();
                }
            }
            None => {
                cr.set_source_rgba(0.5, 0.5, 0.5, 0.3);
                cr.rectangle(0.0, mid - 0.5, width, 1.0);
                let _ = cr.fill();
            }
        }
        // Markers and playhead in the text colour, so they show on light themes too.
        let ink = self.wave.color();
        let (ir, ig, ib) = (
            f64::from(ink.red()),
            f64::from(ink.green()),
            f64::from(ink.blue()),
        );
        if duration > 0 {
            // Chapter markers: a thin line with a small notch at the top.
            for (ms, _) in &state.chapters {
                let x = ((*ms * 1000) as f64 / duration as f64 * width).round() + 0.5;
                cr.set_source_rgba(ir, ig, ib, 0.35);
                cr.rectangle(x - 0.5, 0.0, 1.0, height);
                let _ = cr.fill();
                cr.set_source_rgba(ir, ig, ib, 0.85);
                cr.move_to(x - 4.0, 0.0);
                cr.line_to(x + 4.0, 0.0);
                cr.line_to(x, 5.0);
                cr.close_path();
                let _ = cr.fill();
            }
            cr.set_source_rgba(ir, ig, ib, 0.9);
            cr.rectangle(head.round() - 0.5, 0.0, 1.5, height);
            let _ = cr.fill();
        }
    }
}

/// Decodes `path` at a low rate and keeps the loudest sample per bin, scaled
/// to 0..1 with a gentle curve so quiet speech still shows.
fn peaks(path: &Path) -> Option<Vec<f32>> {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-ac", "1", "-ar", "4000", "-f", "s16le", "-"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let samples: Vec<f32> = output
        .stdout
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| f32::from(i16::from_le_bytes(*b)).abs() / 32768.0)
        .collect();
    if samples.is_empty() {
        return Some(vec![0.0; BINS]);
    }
    let per_bin = samples.len().div_ceil(BINS);
    let mut bins: Vec<f32> = samples
        .chunks(per_bin)
        .map(|chunk| chunk.iter().copied().fold(0.0, f32::max))
        .collect();
    bins.resize(BINS, 0.0);
    let loudest = bins.iter().copied().fold(0.0, f32::max).max(1e-4);
    Some(bins.into_iter().map(|v| (v / loudest).sqrt()).collect())
}

fn clock(secs: i64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_updates_playback_paths_and_preserves_position_and_chapters() {
        let from = Path::new("/meetings/Old");
        let to = Path::new("/meetings/New");
        let mut state = State {
            files: vec![from.join("mic.ogg"), from.join("computer.ogg")],
            paused_at_us: 12_000_000,
            duration_us: 60_000_000,
            chapters: vec![(1000, "Intro".into())],
            ..Default::default()
        };
        state.relocate(from, to);
        assert_eq!(
            state.files,
            vec![to.join("mic.ogg"), to.join("computer.ogg")]
        );
        assert_eq!(state.paused_at_us, 12_000_000);
        assert_eq!(state.duration_us, 60_000_000);
        assert_eq!(state.chapters, vec![(1000, "Intro".into())]);
        state.relocate(to, Path::new("/meetings/Final"));
        assert_eq!(state.files[0], Path::new("/meetings/Final/mic.ogg"));
    }
}
