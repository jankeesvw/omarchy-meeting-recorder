//! The webcam, recorded next to the audio while its toggle is on.
//!
//! ffmpeg takes the camera, picture only, one part for every stretch it runs:
//! a pause ends a part, and so does turning the toggle off. Each part knows
//! where it sits on the audio's clock, the recorded time without the pauses,
//! so `webcam.mp4` keeps time with the audio. A recording, or a resume, with
//! the camera on starts its audio once the first frame is in, so the parts
//! join up and are copied as they are; where the camera was off, or was
//! still starting, black fills the gap and the video is encoded once more.

use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::APP_NAME;

/// The video in a meeting folder, when a webcam was recorded.
pub const FILE: &str = "webcam.mp4";

/// The video devices, as (device, name), sorted by name and then by node.
pub fn devices() -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir("/sys/class/video4linux") else {
        return Vec::new();
    };
    let mut found: Vec<(PathBuf, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = std::fs::read_to_string(entry.path().join("name")).ok()?;
            Some((PathBuf::from("/dev").join(entry.file_name()), name.trim().to_owned()))
        })
        .collect();
    sort(&mut found);
    found
}

/// By name, then by node number, so video9 comes before video10 and each
/// camera starts on its first node, the one with the picture.
fn sort(devices: &mut [(PathBuf, String)]) {
    let node = |device: &PathBuf| device.to_str()?.strip_prefix("/dev/video")?.parse::<u32>().ok();
    devices.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| node(&a.0).cmp(&node(&b.0))));
}

/// The cameras to choose from: each name once, on its first device (a camera
/// has more, for metadata), without dummy loopback devices.
pub fn cameras(devices: &[(PathBuf, String)]) -> Vec<(PathBuf, String)> {
    let mut cameras: Vec<(PathBuf, String)> = Vec::new();
    for (device, name) in devices {
        if !name.starts_with("Dummy video device") && !cameras.iter().any(|(_, n)| n == name) {
            cameras.push((device.clone(), name.clone()));
        }
    }
    cameras
}

/// The camera the webcam toggle records: the one named `saved` when it is
/// there, else the first of `cameras`.
pub fn pick(devices: &[(PathBuf, String)], saved: &str) -> Option<(PathBuf, String)> {
    let cameras = cameras(devices);
    cameras
        .iter()
        .find(|(_, name)| !saved.is_empty() && name == saved)
        .or(cameras.first())
        .cloned()
}

fn segment(staging: &Path, n: u32) -> PathBuf {
    staging.join(format!("webcam-{n:03}.mkv"))
}

/// The preview: a small picture, a dozen frames a second, as JPEGs on stdout.
const PREVIEW: [&str; 10] = [
    "-vf", "scale=192:-2,fps=12", "-c:v", "mjpeg", "-q:v", "7", "-f", "image2pipe", "pipe:1", "-y",
];

/// An ffmpeg that stops with the app, should the app go away mid-recording,
/// instead of filling the disk or keeping the camera on.
fn ffmpeg() -> Command {
    let mut command = Command::new("ffmpeg");
    command
        .args(["-hide_banner", "-loglevel", "error", "-nostats"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    command
}

/// Takes the first whole JPEG off the front of `buffer`, dropping what came before it.
pub(crate) fn take_jpeg(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let start = buffer.windows(2).position(|w| w == [0xFF, 0xD8])?;
    let end = buffer[start + 2..].windows(2).position(|w| w == [0xFF, 0xD9])? + start + 4;
    let frame = buffer[start..end].to_vec();
    buffer.drain(..end);
    Some(frame)
}

/// Hands the frames a process writes to the preview, until its stdout closes.
/// Frames the window has no time for are dropped.
fn read_frames(
    child: &mut Child,
    frames: &async_channel::Sender<Vec<u8>>,
    first: Option<async_channel::Sender<()>>,
) {
    let Some(mut stdout) = child.stdout.take() else { return };
    let frames = frames.clone();
    std::thread::spawn(move || {
        let mut first = first;
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 65536];
        loop {
            let read = match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            buffer.extend_from_slice(&chunk[..read]);
            while let Some(frame) = take_jpeg(&mut buffer) {
                if let Some(first) = first.take() {
                    let _ = first.try_send(());
                }
                let _ = frames.try_send(frame);
            }
        }
    });
}

/// One run of the camera during a recording.
struct Part {
    file: PathBuf,
    /// Where the first and the last frame sit on the audio's clock.
    start: Option<f64>,
    end: Option<f64>,
}

/// What `export` makes `webcam.mp4` from, handed over when a recording stops.
pub struct Plan {
    closing: Vec<Child>,
    parts: Vec<(PathBuf, f64, f64)>,
    total: f64,
}

pub struct Webcam {
    /// The camera while its toggle is on during a recording.
    device: RefCell<Option<PathBuf>>,
    staging: RefCell<Option<PathBuf>>,
    running: RefCell<Option<Child>>,
    /// Parts told to stop that may still be writing their last frames.
    ending: RefCell<Vec<Child>>,
    parts: RefCell<Vec<Part>>,
    /// The audio's clock: the stretches done, and when the running one began.
    recorded: Cell<f64>,
    stretch_began: Cell<Option<Instant>>,
    /// The camera shown, before a recording starts.
    previewing: RefCell<Option<Child>>,
    frames: async_channel::Sender<Vec<u8>>,
    /// Hears when the running part sends its first frame.
    first_frame: RefCell<Option<async_channel::Receiver<()>>>,
}

impl Webcam {
    /// A webcam whose preview frames, JPEGs, go to `frames`.
    pub fn new(frames: async_channel::Sender<Vec<u8>>) -> Self {
        Webcam {
            device: RefCell::default(),
            staging: RefCell::default(),
            running: RefCell::default(),
            ending: RefCell::default(),
            parts: RefCell::default(),
            recorded: Cell::new(0.0),
            stretch_began: Cell::new(None),
            previewing: RefCell::default(),
            frames,
            first_frame: RefCell::default(),
        }
    }

    /// Shows `device` in the preview while nothing records, or stops showing it with None.
    pub fn preview(&self, device: Option<&Path>) {
        let Some(device) = device else {
            self.stop_preview();
            return;
        };
        if self.previewing.borrow().is_some() || self.running.borrow().is_some() {
            return;
        }
        let mut command = ffmpeg();
        command.args(["-f", "v4l2", "-i"]).arg(device).args(PREVIEW).stdin(Stdio::null());
        match command.spawn() {
            Ok(mut child) => {
                read_frames(&mut child, &self.frames, None);
                *self.previewing.borrow_mut() = Some(child);
            }
            Err(err) => eprintln!("{APP_NAME}: could not show the webcam: {err}"),
        }
    }

    /// A camera opens for one program at a time: the preview lets go before recording.
    fn stop_preview(&self) {
        if let Some(mut child) = self.previewing.borrow_mut().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Where the audio is now: the recorded time, without the pauses.
    fn clock(&self) -> f64 {
        self.recorded.get() + self.stretch_began.get().map_or(0.0, |began| began.elapsed().as_secs_f64())
    }

    /// A recording begins in `staging`, with the camera when `device` is given.
    /// The audio has not started yet; see `audio_started`.
    pub fn begin(&self, staging: &Path, device: Option<&Path>) {
        *self.staging.borrow_mut() = Some(staging.to_owned());
        self.parts.borrow_mut().clear();
        self.recorded.set(0.0);
        self.stretch_began.set(None);
        *self.device.borrow_mut() = device.map(Path::to_owned);
        self.start_part();
    }

    /// Starts a part when the camera is wanted and none runs; the same ffmpeg
    /// feeds the preview.
    fn start_part(&self) {
        let (Some(device), Some(staging)) = (self.device.borrow().clone(), self.staging.borrow().clone())
        else {
            return;
        };
        if self.running.borrow().is_some() {
            return;
        }
        self.stop_preview();
        let file = segment(&staging, self.parts.borrow().len() as u32);
        let mut command = ffmpeg();
        command
            .args(["-thread_queue_size", "512", "-f", "v4l2", "-i"])
            .arg(&device)
            .args(["-map", "0:v"])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p", "-y"])
            .arg(&file)
            .args(["-map", "0:v"])
            .args(PREVIEW)
            .stdin(Stdio::piped());
        match command.spawn() {
            Ok(mut child) => {
                let (first, heard) = async_channel::bounded(1);
                read_frames(&mut child, &self.frames, Some(first));
                *self.first_frame.borrow_mut() = Some(heard);
                *self.running.borrow_mut() = Some(child);
                self.parts.borrow_mut().push(Part { file, start: None, end: None });
            }
            Err(err) => eprintln!("{APP_NAME}: could not start the webcam: {err}"),
        }
    }

    /// Waits for the first frame of the part just started, at most `limit`;
    /// at once when none is starting.
    pub async fn first_frame(&self, limit: Duration) {
        let Some(heard) = self.first_frame.borrow_mut().take() else {
            return;
        };
        let _ = gtk::glib::future_with_timeout(limit, heard.recv()).await;
        self.mark_start();
    }

    /// The running part begins here on the audio's clock, if the audio runs.
    fn mark_start(&self) {
        if self.stretch_began.get().is_none() {
            return;
        }
        let clock = self.clock();
        if let Some(part) = self.parts.borrow_mut().last_mut()
            && part.start.is_none()
            && part.end.is_none()
        {
            part.start = Some(clock);
        }
    }

    /// The audio starts or resumes now.
    pub fn audio_started(&self) {
        self.stretch_began.set(Some(Instant::now()));
        // A part whose first frame is already in begins with the audio.
        if self.first_frame.borrow().is_none() {
            self.mark_start();
        }
    }

    /// The audio pauses: the running part ends where the audio stops.
    pub fn audio_paused(&self) {
        self.end_part();
        self.recorded.set(self.clock());
        self.stretch_began.set(None);
    }

    /// Resuming: the camera comes back first, when it is wanted, so the audio
    /// can wait for its first frame.
    pub fn resume(&self) {
        self.start_part();
    }

    /// The toggle, during a recording: on starts a part (the gap before its
    /// first frame becomes black), off ends the running one.
    pub fn switch(&self, device: Option<&Path>) {
        *self.device.borrow_mut() = device.map(Path::to_owned);
        if device.is_some() {
            if self.stretch_began.get().is_some() {
                self.start_part();
            }
        } else {
            self.end_part();
        }
    }

    /// Ends the running part on the audio's clock: ffmpeg closes the file once it reads q.
    fn end_part(&self) {
        let clock = self.clock();
        if let Some(part) = self.parts.borrow_mut().last_mut()
            && part.end.is_none()
        {
            part.end = Some(clock);
        }
        *self.first_frame.borrow_mut() = None;
        if let Some(mut child) = self.running.borrow_mut().take() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(b"q");
            }
            self.ending.borrow_mut().push(child);
        }
    }

    /// Ends the recording, and hands over what `export` needs.
    pub fn end(&self) -> Plan {
        self.end_part();
        let total = self.clock();
        self.stretch_began.set(None);
        *self.device.borrow_mut() = None;
        let parts = self
            .parts
            .take()
            .into_iter()
            .filter_map(|part| Some((part.file, part.start?, part.end?)))
            .filter(|(_, start, end)| end > start)
            .collect();
        Plan {
            closing: self.ending.take(),
            parts,
            total,
        }
    }
}

impl Drop for Webcam {
    /// A closed window turns its camera off.
    fn drop(&mut self) {
        self.stop_preview();
        for mut child in self.running.take().into_iter().chain(self.ending.take()) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// How far a part may sit from the next one and still count as joined up.
const SLACK: f64 = 0.1;

/// Whether the parts cover the whole audio without gaps.
fn joined_up(parts: &[(PathBuf, f64, f64)], total: f64) -> bool {
    let Some(first) = parts.first() else { return false };
    let last = &parts[parts.len() - 1];
    first.1 <= SLACK
        && parts.windows(2).all(|pair| pair[1].1 - pair[0].2 <= SLACK)
        && total - last.2 <= SLACK
}

/// The ffmpeg concat list for parts that join up: each up to its length.
fn concat_list(parts: &[(PathBuf, f64, f64)]) -> String {
    parts
        .iter()
        .map(|(path, start, end)| {
            let path = path.display().to_string().replace('\'', "'\\''");
            format!("file '{path}'\noutpoint {:.3}\n", end - start)
        })
        .collect()
}

/// The filter that lays the parts on the audio's clock, black in between.
/// Input n is the nth part.
fn padded_filter(parts: &[(PathBuf, f64, f64)], total: f64) -> String {
    const SIZE: &str = "1920x1080";
    let mut steps = Vec::new();
    let mut pieces = Vec::new();
    let mut at = 0.0;
    fn black(length: f64, steps: &mut Vec<String>, pieces: &mut Vec<String>) {
        let label = format!("b{}", pieces.len());
        steps.push(format!("color=c=black:s={SIZE}:r=30:d={length:.3}[{label}]"));
        pieces.push(label);
    }
    for (n, (_, start, end)) in parts.iter().enumerate() {
        if start - at > 0.04 {
            black(start - at, &mut steps, &mut pieces);
        }
        let label = format!("p{n}");
        steps.push(format!(
            "[{n}:v]trim=duration={:.3},setpts=PTS-STARTPTS,scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,fps=30,format=yuv420p,setsar=1[{label}]",
            end - start
        ));
        pieces.push(label);
        at = *end;
    }
    if total - at > 0.04 {
        black(total - at, &mut steps, &mut pieces);
    }
    let inputs: String = pieces.iter().map(|label| format!("[{label}]")).collect();
    steps.push(format!("{inputs}concat=n={}:v=1:a=0[v]", pieces.len()));
    steps.join(";")
}

/// Waits for the last parts to close and makes `webcam.mp4` in `out`, as long
/// as the audio. Blocking. False when the camera recorded nothing.
pub fn export(staging: &Path, out: &Path, plan: Plan) -> bool {
    for mut child in plan.closing {
        let deadline = Instant::now() + Duration::from_secs(10);
        while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    let parts: Vec<(PathBuf, f64, f64)> = plan
        .parts
        .into_iter()
        .filter(|(path, _, _)| std::fs::metadata(path).is_ok_and(|m| m.len() > 0))
        .collect();
    if parts.is_empty() {
        return false;
    }
    let mut command = Command::new("ffmpeg");
    command.args(["-hide_banner", "-loglevel", "error", "-y"]).stdin(Stdio::null());
    if joined_up(&parts, plan.total) {
        let list = staging.join("webcam.txt");
        if std::fs::write(&list, concat_list(&parts)).is_err() {
            return false;
        }
        command
            .args(["-f", "concat", "-safe", "0", "-i"])
            .arg(&list)
            .args(["-c", "copy"]);
    } else {
        for (path, _, _) in &parts {
            command.arg("-i").arg(path);
        }
        command
            .args(["-filter_complex", &padded_filter(&parts, plan.total), "-map", "[v]"])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20"]);
    }
    command
        .args(["-movflags", "+faststart"])
        .arg(out.join(FILE))
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_toggle_records_a_real_camera() {
        let devices = vec![
            (PathBuf::from("/dev/video0"), "Dummy video device (0x0000)".to_owned()),
            (PathBuf::from("/dev/video9"), "Opal C1".to_owned()),
            (PathBuf::from("/dev/video2"), "Laptop camera".to_owned()),
        ];
        assert_eq!(pick(&devices, "").map(|d| d.1), Some("Opal C1".to_owned()));
        assert_eq!(pick(&devices, "Laptop camera").map(|d| d.1), Some("Laptop camera".to_owned()));
        assert_eq!(pick(&devices, "Gone").map(|d| d.1), Some("Opal C1".to_owned()));
        assert_eq!(pick(&devices[..1], ""), None);
    }

    #[test]
    fn each_camera_is_offered_once_on_its_first_device() {
        let devices = vec![
            (PathBuf::from("/dev/video0"), "Laptop camera".to_owned()),
            (PathBuf::from("/dev/video1"), "Laptop camera".to_owned()),
            (PathBuf::from("/dev/video8"), "Dummy video device (0x0000)".to_owned()),
            (PathBuf::from("/dev/video4"), "Opal C1".to_owned()),
            (PathBuf::from("/dev/video5"), "Opal C1".to_owned()),
        ];
        let offered: Vec<_> = cameras(&devices).into_iter().map(|(d, _)| d).collect();
        assert_eq!(offered, [PathBuf::from("/dev/video0"), PathBuf::from("/dev/video4")]);
        assert_eq!(pick(&devices, "Opal C1").map(|d| d.0), Some(PathBuf::from("/dev/video4")));
    }

    #[test]
    fn video9_comes_before_video10() {
        let mut devices = vec![
            (PathBuf::from("/dev/video10"), "Opal C1".to_owned()),
            (PathBuf::from("/dev/video9"), "Opal C1".to_owned()),
            (PathBuf::from("/dev/video2"), "Laptop camera".to_owned()),
        ];
        sort(&mut devices);
        let offered: Vec<_> = cameras(&devices).into_iter().map(|(d, _)| d).collect();
        assert_eq!(offered, [PathBuf::from("/dev/video2"), PathBuf::from("/dev/video9")]);
    }

    #[test]
    fn frames_are_cut_out_of_the_stream() {
        let mut stream = vec![0x00, 0xFF, 0xD8, 1, 2, 0xFF, 0xD9, 0xFF, 0xD8, 3];
        assert_eq!(take_jpeg(&mut stream), Some(vec![0xFF, 0xD8, 1, 2, 0xFF, 0xD9]));
        // The next frame is not whole yet: it waits for more of the stream.
        assert_eq!(take_jpeg(&mut stream), None);
        assert_eq!(stream, vec![0xFF, 0xD8, 3]);
    }

    fn part(n: u32, start: f64, end: f64) -> (PathBuf, f64, f64) {
        (segment(Path::new("/tmp/rec"), n), start, end)
    }

    #[test]
    fn parts_that_join_up_are_copied_each_up_to_its_length() {
        let parts = [part(0, 0.0, 4.25), part(1, 4.3, 6.8)];
        assert!(joined_up(&parts, 6.85));
        assert_eq!(
            concat_list(&parts),
            "file '/tmp/rec/webcam-000.mkv'\noutpoint 4.250\nfile '/tmp/rec/webcam-001.mkv'\noutpoint 2.500\n"
        );
    }

    #[test]
    fn a_camera_turned_on_later_or_off_early_is_padded_with_black() {
        // On after 2 s, off at 5 s, of 8 s of audio.
        let parts = [part(0, 2.0, 5.0)];
        assert!(!joined_up(&parts, 8.0));
        let filter = padded_filter(&parts, 8.0);
        assert!(filter.starts_with("color=c=black:s=1920x1080:r=30:d=2.000[b0];[0:v]trim=duration=3.000"));
        assert!(filter.ends_with("color=c=black:s=1920x1080:r=30:d=3.000[b2];[b0][p0][b2]concat=n=3:v=1:a=0[v]"));
    }

    #[test]
    fn segments_are_numbered_in_order() {
        let staging = Path::new("/tmp/rec");
        assert_eq!(segment(staging, 0), Path::new("/tmp/rec/webcam-000.mkv"));
        assert_eq!(segment(staging, 12), Path::new("/tmp/rec/webcam-012.mkv"));
    }
}
