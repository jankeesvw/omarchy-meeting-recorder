//! The player on the done page: a two-lane waveform of the meeting (you above
//! the line, the other side below it) with a playhead, click or drag to seek.
//!
//! Playback is `ffmpeg` decoding into `pacat`, not GStreamer: a stock Omarchy
//! has no GStreamer audio sink, while `pacat` comes with the same package as
//! the `parec` the recorder already records with. Pausing stops the pipeline
//! and playing starts it again at the position; a meeting saved as separate
//! files is mixed on the fly. A meeting with a `webcam.mp4` shows it small
//! (`video_widget`, which the page lays over the transcript): a second ffmpeg decodes its frames ahead, each
//! stamped with its time, and the frame whose time the audio has reached is
//! shown; a still of the position stands in while paused.

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
type TrimCallback = Rc<RefCell<Option<Box<dyn Fn(i64, i64, i64)>>>>;

/// Frames of the webcam a second ahead, as (time in µs, JPEG).
type VideoFrames = async_channel::Receiver<(i64, Vec<u8>)>;

/// A running `ffmpeg | pacat` pipeline, stopped when dropped, with the
/// webcam's frames alongside when there is a video.
struct Playback {
    ffmpeg: Child,
    pacat: Child,
    video: Option<(Child, VideoFrames)>,
    /// A frame read ahead whose time has not come yet.
    next_frame: Option<(i64, Vec<u8>)>,
    started: Instant,
    from_us: i64,
}

impl Playback {
    fn start(files: &[PathBuf], from_us: i64, video: Option<&Path>) -> Option<Playback> {
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
                video: video.and_then(|path| play_video(path, from_us)),
                next_frame: None,
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

    /// The newest frame whose time the audio has reached, if a new one is due.
    fn due_frame(&mut self) -> Option<Vec<u8>> {
        let now = self.position_us();
        let (_, frames) = self.video.as_ref()?;
        let mut due = None;
        while let Some((at, frame)) = self.next_frame.take().or_else(|| frames.try_recv().ok()) {
            if at > now {
                self.next_frame = Some((at, frame));
                break;
            }
            due = Some(frame);
        }
        due
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        let _ = self.ffmpeg.kill();
        let _ = self.pacat.kill();
        let _ = self.ffmpeg.wait();
        let _ = self.pacat.wait();
        if let Some((video, _)) = self.video.as_mut() {
            let _ = video.kill();
            let _ = video.wait();
        }
    }
}

/// Frames per second of the small webcam picture.
const VIDEO_FPS: i64 = 15;

/// Decodes the webcam from `from_us` on, a second ahead of the audio at
/// most: frame n of the even stream the fps filter makes is at
/// `from_us + n / VIDEO_FPS`.
fn play_video(path: &Path, from_us: i64) -> Option<(Child, VideoFrames)> {
    let mut command = Command::new("ffmpeg");
    command
        .args(["-v", "error", "-nostdin", "-ss", &format!("{:.3}", from_us as f64 / 1_000_000.0), "-i"])
        .arg(path)
        .args(["-an", "-vf", &format!("scale=480:-2,fps={VIDEO_FPS}"), "-c:v", "mjpeg", "-q:v", "6"])
        .args(["-f", "image2pipe", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = die_with_parent(&mut command).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (frames, received) = async_channel::bounded(VIDEO_FPS as usize);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 65536];
        let mut n: i64 = 0;
        while let Ok(read) = stdout.read(&mut chunk) {
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
            while let Some(frame) = crate::webcam::take_jpeg(&mut buffer) {
                // Waits while a second of frames is queued: ffmpeg waits with it.
                if frames.send_blocking((from_us + n * 1_000_000 / VIDEO_FPS, frame)).is_err() {
                    return;
                }
                n += 1;
            }
        }
    });
    Some((child, received))
}

/// One frame of the webcam at `us`, as a JPEG, for while nothing plays.
fn still(path: &Path, us: i64) -> Option<Vec<u8>> {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-ss", &format!("{:.3}", us as f64 / 1_000_000.0), "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-vf", "scale=480:-2", "-c:v", "mjpeg", "-q:v", "4"])
        .args(["-f", "image2pipe", "pipe:1"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    (output.status.success() && !output.stdout.is_empty()).then_some(output.stdout)
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
    /// The meeting's webcam.mp4, when it has one.
    video: Option<PathBuf>,
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
    selection: Option<(i64, i64)>,
}

impl State {
    fn relocate(&mut self, from: &Path, to: &Path) {
        for path in self.files.iter_mut().chain(self.video.as_mut()) {
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
    /// The webcam, small, when the meeting has one.
    video: gtk::Picture,
    video_box: adw::Clamp,
    frames: async_channel::Sender<Vec<u8>>,
    wave: gtk::DrawingArea,
    time: gtk::Label,
    state: Rc<RefCell<State>>,
    /// Called with the position in ms while playing, for the transcript highlight.
    on_position: PositionCallback,
    on_trim: TrimCallback,
    trim: gtk::Button,
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

        let video = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Cover)
            .can_shrink(true)
            .css_classes(["player-video"])
            .build();
        video.set_overflow(gtk::Overflow::Hidden);
        // Small, whatever size the frames are: they are wider, for a sharp picture on HiDPI.
        let video_box = adw::Clamp::builder()
            .maximum_size(240)
            .tightening_threshold(240)
            .child(&video)
            .visible(false)
            .build();
        let (frames, frames_rx) = async_channel::bounded::<Vec<u8>>(2);
        let picture = video.clone();
        glib::spawn_future_local(async move {
            while let Ok(frame) = frames_rx.recv().await {
                if let Ok(texture) = gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(frame)) {
                    picture.set_paintable(Some(&texture));
                }
            }
        });

        root.append(&button);
        root.append(&wave);
        root.append(&time);
        let trim = gtk::Button::builder()
            .icon_name("edit-cut-symbolic")
            .tooltip_text("Save selected range and preserve the other sections")
            .visible(false)
            .css_classes(["circular", "flat"])
            .sensitive(false)
            .build();
        root.append(&trim);

        let player = Player {
            root,
            button,
            video,
            video_box,
            frames,
            wave,
            time,
            state: Rc::default(),
            on_position: Rc::default(),
            on_trim: Rc::default(),
            trim,
            ticking: Rc::default(),
        };

        let this = player.clone();
        player.wave.set_draw_func(move |_, cr, width, height| {
            this.draw(cr, f64::from(width), f64::from(height));
        });

        let this = player.clone();
        player.button.connect_clicked(move |_| this.toggle());
        let this = player.clone();
        player.trim.connect_clicked(move |_| {
            this.pause();
            if let Some(callback) = this.on_trim.borrow().as_ref() {
                let state = this.state.borrow();
                if let Some((start, end)) = state.selection {
                    callback(start, end, state.duration_us);
                }
            }
        });

        // Click or drag anywhere on the waveform to seek.
        let drag = gtk::GestureDrag::new();
        let this = player.clone();
        drag.connect_drag_begin(move |_, x, _| {
            this.state.borrow_mut().selection = None;
            this.seek_to_x(x);
        });
        let this = player.clone();
        drag.connect_drag_update(move |gesture, dx, _| {
            if let Some((x, _)) = gesture.start_point() {
                if dx.abs() >= 4.0 {
                    if this.is_playing() { this.pause(); }
                    let a = this.time_at_x(x);
                    let b = this.time_at_x(x + dx);
                    this.state.borrow_mut().selection = Some((a.min(b), a.max(b)));
                    this.refresh();
                }
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

    /// The webcam picture, for the page to lay where it fits; hidden without a video.
    pub fn video_widget(&self) -> &adw::Clamp {
        &self.video_box
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn connect_position(&self, callback: impl Fn(i64) + 'static) {
        *self.on_position.borrow_mut() = Some(Box::new(callback));
    }

    pub fn connect_trim(&self, callback: impl Fn(i64, i64, i64) + 'static) {
        *self.on_trim.borrow_mut() = Some(Box::new(callback));
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
            state.video = Some(dir.join(crate::webcam::FILE)).filter(|path| path.is_file());
            state.paused_at_us = 0;
            state.generation += 1;
            state.generation
        };
        self.root.set_visible(!playable.is_empty());
        self.video_box.set_visible(self.state.borrow().video.is_some());
        self.show_still(0);

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
        state.video = None;
        self.video_box.set_visible(false);
        self.video.set_paintable(None::<&gtk::gdk::Paintable>);
        state.duration_us = 0;
        state.paused_at_us = 0;
        state.peaks = None;
        state.chapters.clear();
        state.selection = None;
        self.trim.set_visible(false);
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
            let video = state.video.clone();
            state.playback = Playback::start(&state.files, state.paused_at_us, video.as_deref());
            state.playback.is_some()
        };
        self.set_playing_icon(started);
        if started {
            self.start_ticking();
            self.start_showing_video();
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
        self.show_still(position);
        self.refresh();
    }

    /// While playing, puts the frame the audio has reached on screen, thirty times a second.
    fn start_showing_video(&self) {
        if self.state.borrow().video.is_none() {
            return;
        }
        let this = self.clone();
        glib::timeout_add_local(Duration::from_millis(33), move || {
            let mut state = this.state.borrow_mut();
            let Some(playback) = state.playback.as_mut() else {
                return glib::ControlFlow::Break;
            };
            if playback.video.is_none() {
                return glib::ControlFlow::Break;
            }
            if let Some(frame) = playback.due_frame()
                && let Ok(texture) = gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(frame))
            {
                this.video.set_paintable(Some(&texture));
            }
            glib::ControlFlow::Continue
        });
    }

    /// Shows the webcam at `us` while nothing plays.
    fn show_still(&self, us: i64) {
        let Some(path) = self.state.borrow().video.clone() else {
            return;
        };
        let frames = self.frames.clone();
        glib::spawn_future_local(async move {
            if let Ok(Some(frame)) = gio::spawn_blocking(move || still(&path, us)).await {
                let _ = frames.send(frame).await;
            }
        });
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
        } else {
            self.show_still(us);
        }
        self.refresh();
    }

    fn seek_to_x(&self, x: f64) {
        self.seek(self.time_at_x(x));
    }

    fn time_at_x(&self, x: f64) -> i64 {
        let width = f64::from(self.wave.width()).max(1.0);
        let fraction = (x / width).clamp(0.0, 1.0);
        (self.duration_us() as f64 * fraction) as i64
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
        let selection = self.state.borrow().selection;
        self.trim.set_visible(selection.is_some());
        self.trim.set_sensitive(selection.is_some_and(|(start, end)| end > start));
        self.wave.set_tooltip_text(Some("Click to seek. Drag to select a section to keep."));
        self.time.set_label(&format!(
            "{} / {}",
            clock(position / 1_000_000),
            clock(duration / 1_000_000)
        ));
        if let Some((start, end)) = selection {
            self.time.set_label(&format!("{} to {}", clock(start / 1_000_000), clock(end / 1_000_000)));
        }
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
        if let Some((start, end)) = state.selection.filter(|_| duration > 0) {
            let left = start as f64 / duration as f64 * width;
            let right = end as f64 / duration as f64 * width;
            cr.set_source_rgba(0.3, 0.65, 1.0, 0.25);
            cr.rectangle(left, 0.0, right - left, height);
            let _ = cr.fill();
            cr.set_source_rgba(0.3, 0.65, 1.0, 1.0);
            cr.rectangle(left, 0.0, 2.0, height);
            cr.rectangle(right - 2.0, 0.0, 2.0, height);
            let _ = cr.fill();
        }

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
