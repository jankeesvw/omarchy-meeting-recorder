//! Live state for the bar widget.
//!
//! The app listens on a Unix socket in $XDG_RUNTIME_DIR and writes one JSON
//! line per tick to every connected client: 20 times a second while recording,
//! once a second otherwise. `omarchy-meeting-recorder watch` connects to it and
//! copies those lines to stdout, printing `{"state":"off"}` while the app is not
//! running, so the widget only has to read NDJSON from a process.
//!
//! A line looks like:
//! {"state":"recording","elapsed":754,"title":"Weekly","mic":0.62,"computer":0.31,"progress":0.0}
//! with `mic` and `computer` as meter levels from 0 to 1, and `progress` the
//! transcription progress from 0 to 1 while the state is "transcribing".

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gtk::glib;

use crate::APP_NAME;
use crate::audio::{Source, to_meter};

const MAX_LINE: usize = 4096;

#[derive(Clone, Default)]
pub struct Status {
    /// idle, recording, paused, stopping, transcribing or done
    pub state: &'static str,
    pub started_at: i64,
    /// Seconds spent paused so far, and when the current pause began (0: not paused).
    pub paused_secs: i64,
    pub pause_began: i64,
    pub title: String,
    pub progress: f64,
}

pub type SharedStatus = Arc<Mutex<Status>>;

fn socket_path() -> PathBuf {
    glib::user_runtime_dir().join(format!("{APP_NAME}.sock"))
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Commands a client may send, one per line.
#[derive(Debug, PartialEq)]
pub enum Control {
    Start,
    AutoStart(Option<String>),
    Stop,
    Compact,
    Pause,
}

fn parse_command(line: &str) -> Option<Control> {
    match line.trim() {
        "start" => Some(Control::Start),
        "stop" => Some(Control::Stop),
        "compact" => Some(Control::Compact),
        "pause" => Some(Control::Pause),
        other => {
            let value: serde_json::Value = serde_json::from_str(other).ok()?;
            if value["command"].as_str()? != "auto-start" {
                return None;
            }
            if value["title"].is_null() {
                return Some(Control::AutoStart(None));
            }
            let title = value["title"].as_str()?.trim();
            if title.is_empty()
                || title.chars().count() > 200
                || title.chars().any(char::is_control)
            {
                return None;
            }
            Some(Control::AutoStart(Some(title.to_owned())))
        }
    }
}

/// Read one status snapshot without waiting indefinitely for the GUI.
pub fn snapshot() -> Result<serde_json::Value, String> {
    let stream = UnixStream::connect(socket_path()).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(MAX_LINE as u64)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&line).map_err(|e| e.to_string())
}

pub fn auto_start(title: Option<&str>) -> bool {
    send(&serde_json::json!({"command": "auto-start", "title": title}).to_string())
}

/// Starts the socket server. Called once, from the primary instance. Clients
/// get the state lines; a valid control command a client writes is
/// passed on to `commands`.
pub fn serve(
    status: SharedStatus,
    mic: Source,
    system: Source,
    commands: async_channel::Sender<Control>,
) {
    let path = socket_path();
    // A socket file left behind by a crash refuses new binds; nobody answers on it.
    if UnixStream::connect(&path).is_err() {
        let _ = std::fs::remove_file(&path);
    }
    let Ok(listener) = UnixListener::bind(&path) else {
        eprintln!("{APP_NAME}: could not listen on {}", path.display());
        return;
    };
    let clients: Arc<Mutex<Vec<UnixStream>>> = Arc::default();

    let accepted = clients.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            // A write timeout rather than non-blocking mode: the flag would be
            // shared with the reading clone below.
            if stream
                .set_write_timeout(Some(Duration::from_millis(20)))
                .is_err()
            {
                continue;
            }
            if let Ok(reader) = stream.try_clone() {
                let commands = commands.clone();
                thread::spawn(move || read_commands(reader, &commands));
            }
            accepted.lock().unwrap().push(stream);
        }
    });

    thread::spawn(move || {
        loop {
            let snapshot = status.lock().unwrap().clone();
            let recording = snapshot.state == "recording";
            let taking = recording || snapshot.state == "paused";
            let until = if snapshot.pause_began > 0 {
                snapshot.pause_began
            } else {
                now()
            };
            let busy = recording || snapshot.state == "transcribing";
            let line = serde_json::json!({
                "state": snapshot.state,
                "elapsed": if taking { (until - snapshot.started_at - snapshot.paused_secs).max(0) } else { 0 },
                "title": snapshot.title,
                "mic": round(to_meter(mic.recent_peak(3))),
                "computer": round(to_meter(system.recent_peak(3))),
                "progress": round(snapshot.progress),
            })
            .to_string()
                + "\n";
            // A client that cannot keep up is dropped rather than waited for.
            clients
                .lock()
                .unwrap()
                .retain_mut(|client| match client.write_all(line.as_bytes()) {
                    Ok(()) => true,
                    Err(e) => e.kind() == ErrorKind::Interrupted,
                });
            thread::sleep(Duration::from_millis(if recording {
                50
            } else if busy {
                250
            } else {
                1000
            }));
        }
    });
}

fn read_commands(stream: UnixStream, commands: &async_channel::Sender<Control>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.by_ref().take(MAX_LINE as u64).read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) if !line.ends_with('\n') => return,
            Ok(_) => {
                if let Some(command) = parse_command(&line) {
                    let _ = commands.send_blocking(command);
                }
            }
        }
    }
}

/// `omarchy-meeting-recorder stop`: ask the running app to stop recording.
pub fn send(command: &str) -> bool {
    match UnixStream::connect(socket_path()) {
        Ok(mut stream) => stream.write_all(format!("{command}\n").as_bytes()).is_ok(),
        Err(_) => false,
    }
}

fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// `omarchy-meeting-recorder watch`: relay the app's state lines to stdout.
pub fn watch() {
    let mut stdout = std::io::stdout();
    loop {
        if let Ok(stream) = UnixStream::connect(socket_path()) {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.by_ref().take(MAX_LINE as u64).read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if !line.ends_with('\n') => break, // over-long line: not ours
                    Ok(_) => {
                        if stdout
                            .write_all(line.as_bytes())
                            .and_then(|_| stdout.flush())
                            .is_err()
                        {
                            return; // the widget went away
                        }
                    }
                }
            }
        }
        if writeln!(stdout, r#"{{"state":"off"}}"#)
            .and_then(|_| stdout.flush())
            .is_err()
        {
            return;
        }
        thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_commands_still_work() {
        assert_eq!(parse_command("start\n"), Some(Control::Start));
        assert_eq!(parse_command("stop"), Some(Control::Stop));
        assert_eq!(parse_command("pause"), Some(Control::Pause));
        assert_eq!(parse_command("compact"), Some(Control::Compact));
    }

    #[test]
    fn auto_start_preserves_unicode_and_quotes() {
        let title = "Team’s \"weekly\" meeting";
        let line = serde_json::json!({"command":"auto-start", "title":title}).to_string();
        assert_eq!(
            parse_command(&line),
            Some(Control::AutoStart(Some(title.into())))
        );
    }

    #[test]
    fn auto_start_can_leave_naming_to_the_app() {
        assert_eq!(
            parse_command(r#"{"command":"auto-start","title":null}"#),
            Some(Control::AutoStart(None))
        );
    }

    #[test]
    fn invalid_commands_and_titles_are_rejected() {
        for line in ["unknown", "{}", r#"{"command":"stop","title":"meeting"}"#] {
            assert_eq!(parse_command(line), None);
        }
        for title in [String::new(), " ".into(), "x".repeat(201), "a\nb".into()] {
            assert_eq!(
                parse_command(
                    &serde_json::json!({"command":"auto-start", "title":title}).to_string()
                ),
                None
            );
        }
    }
}
