//! Opt-in background recording automation, independent of title suggestions.
use std::collections::HashMap;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::process::Command;
use std::thread;
use std::time::Duration;

use crate::{
    APP_NAME, ipc,
    meeting_detection::{self, DetectedMeeting},
};
use gtk::glib;
use serde_json::Value;

const SERVICE: &str = "omarchy-meeting-recorder-auto-record.service";
const LAUNCH_RETRY_SECS: i64 = 30;

struct Handled {
    window: String,
    retry_after: Option<i64>,
}

#[derive(Default)]
struct Seen {
    pending: Option<(String, Option<String>, usize)>,
    // Suppress repeats for the lifetime of the meeting window, including tab
    // switches and watcher restarts. Only definite launch failures expire.
    handled: HashMap<String, Handled>,
}

impl Seen {
    fn launch_failed(&mut self, key: &str, now: i64) {
        if let Some(entry) = self.handled.get_mut(key) {
            entry.retry_after = Some(now.saturating_add(LAUNCH_RETRY_SECS));
        }
    }

    fn update(&mut self, clients: &[Value]) -> Option<DetectedMeeting> {
        self.update_at(clients, ipc::now())
    }

    fn update_at(&mut self, clients: &[Value], now: i64) -> Option<DetectedMeeting> {
        self.handled.retain(|_, entry| {
            entry.retry_after.is_none_or(|retry| retry > now)
                && clients
                    .iter()
                    .any(|c| c["address"].as_str() == Some(entry.window.as_str()))
        });
        let Some(m) = meeting_detection::unique(clients) else {
            self.pending = None;
            return None;
        };
        // Upgrade an old fallback suppression without changing its retry policy.
        if m.key != m.fallback_key
            && let Some(entry) = self.handled.remove(&m.fallback_key)
        {
            self.handled.entry(m.key.clone()).or_insert(entry);
        }
        if self.handled.contains_key(&m.key) {
            self.pending = None;
            return None;
        }
        let count = match &self.pending {
            Some((key, title, count)) if key == &m.key && title == &m.title => count + 1,
            _ => 1,
        };
        self.pending = Some((m.key.clone(), m.title.clone(), count));
        if count < 2 {
            return None;
        }
        self.handled.insert(
            m.key.clone(),
            Handled {
                window: m.window.clone(),
                retry_after: None,
            },
        );
        self.pending = None;
        Some(m)
    }

    fn restore(text: &str, session: &str) -> Self {
        Self::restore_at(text, session, ipc::now())
    }

    fn restore_at(text: &str, session: &str, now: i64) -> Self {
        let mut seen = Self::default();
        if let Ok(value) = serde_json::from_str::<Value>(text)
            && value["session"].as_str() == Some(session)
            && let Some(handled) = value["handled"].as_object()
        {
            for (key, value) in handled {
                // Old two-hour deadlines are deliberately discarded: an expired
                // entry may still belong to a long call the user already stopped.
                let entry = value.as_str().map(|window| (window, None));
                let entry = entry.or_else(|| {
                    let window = value["window"].as_str()?;
                    if value.get("retry_after").is_some() {
                        let retry = match &value["retry_after"] {
                            Value::Null => None,
                            value => {
                                Some(value.as_i64()?.min(now.saturating_add(LAUNCH_RETRY_SECS)))
                            }
                        };
                        Some((window, retry))
                    } else {
                        value["expires_at"].as_i64()?;
                        Some((window, None))
                    }
                });
                if let Some((window, retry_after)) = entry {
                    seen.handled.insert(
                        key.clone(),
                        Handled {
                            window: window.into(),
                            retry_after,
                        },
                    );
                }
            }
        }
        seen
    }

    fn saved(&self, session: &str) -> String {
        let handled: serde_json::Map<String, Value> = self
            .handled
            .iter()
            .map(|(key, entry)| {
                (
                    key.clone(),
                    serde_json::json!({"window":entry.window,"retry_after":entry.retry_after}),
                )
            })
            .collect();
        serde_json::json!({"session":session,"handled":handled}).to_string()
    }
}

fn save_seen(text: &str) -> Result<(), String> {
    let path = glib::user_runtime_dir().join("omarchy-meeting-recorder-detected.json");
    let temp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| e.to_string())?;
    file.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(temp, path).map_err(|e| e.to_string())
}

// Linux reports an atomically replaced executable with a " (deleted)" suffix.
// Launch the installed replacement, never that procfs display name.
fn installed_executable(path: std::path::PathBuf) -> Result<std::path::PathBuf, String> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    if path.is_file() {
        return Ok(path);
    }
    if let Some(original) = path.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
        let replacement = std::path::PathBuf::from(std::ffi::OsString::from_vec(original.to_vec()));
        if replacement.is_file() {
            return Ok(replacement);
        }
    }
    Err("Recorder executable is no longer installed".into())
}

fn executable() -> Result<std::path::PathBuf, String> {
    installed_executable(std::env::current_exe().map_err(|e| e.to_string())?)
}

enum StartOutcome {
    Started,
    Busy,
    LaunchFailed(String),
}

fn start(title: Option<&str>) -> Result<StartOutcome, String> {
    let mut status = ipc::snapshot();
    if status.is_err() {
        let exe = match executable() {
            Ok(exe) => exe,
            Err(e) => return Ok(StartOutcome::LaunchFailed(e)),
        };
        // A separate unit keeps the GUI/recording alive when the user disables
        // the watcher (systemd otherwise kills all of the watcher's children).
        let output = Command::new("systemd-run")
            .args([
                "--user",
                "--collect",
                "--quiet",
                "--property=Type=exec",
                "--property=PartOf=graphical-session.target",
                "--",
            ])
            .arg(exe)
            .output();
        let output = match output {
            Ok(output) => output,
            Err(e) => return Ok(StartOutcome::LaunchFailed(e.to_string())),
        };
        if !output.status.success() {
            return Ok(StartOutcome::LaunchFailed(format!(
                "Could not open the recorder: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(250));
            status = ipc::snapshot();
            if status.is_ok() {
                break;
            }
        }
    }
    let status = status.map_err(|e| format!("Recorder did not become ready: {e}"))?;
    if !matches!(status["state"].as_str(), Some("idle" | "done")) {
        return Ok(StartOutcome::Busy);
    }
    if !ipc::auto_start(title) {
        return Err("Could not send the recording command".into());
    }
    for _ in 0..5 {
        let status = ipc::snapshot()?;
        if matches!(status["state"].as_str(), Some("recording" | "paused")) {
            return Ok(StartOutcome::Started);
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err("Recording did not start; check the recorder window".into())
}

pub fn run(args: &[String]) -> glib::ExitCode {
    if args == ["--check"] {
        return meeting_detection::check();
    }
    if !args.is_empty() {
        eprintln!("Usage: {APP_NAME} auto-record [--check]");
        return glib::ExitCode::from(2);
    }
    // Only one watcher may consume meeting windows in a desktop session.
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(glib::user_runtime_dir().join("omarchy-meeting-recorder-auto-record.lock"));
    let Ok(_lock) = lock else {
        eprintln!("Could not open the automatic recording lock");
        return glib::ExitCode::FAILURE;
    };
    // SAFETY: the file descriptor stays open for the lifetime of this watcher.
    if unsafe { libc::flock(_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        eprintln!("An automatic recording watcher is already running");
        return glib::ExitCode::FAILURE;
    }
    let session = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap_or_default();
    if session.is_empty() {
        eprintln!("Automatic recording requires a Hyprland session");
        return glib::ExitCode::FAILURE;
    }
    let path = glib::user_runtime_dir().join("omarchy-meeting-recorder-detected.json");
    let previous = std::fs::read_to_string(path).unwrap_or_default();
    let mut seen = Seen::restore(&previous, &session);
    // Persist migrations/closed windows on the first successful window query.
    let mut saved = previous;
    let mut last_error = String::new();
    loop {
        match meeting_detection::clients() {
            Ok(clients) => {
                last_error.clear();
                let detected = seen.update(&clients);
                let next = seen.saved(&session);
                if next != saved {
                    // Persist before starting so a watcher restart cannot start
                    // a meeting again after the user manually stopped it.
                    if let Err(e) = save_seen(&next) {
                        eprintln!("Could not remember detected meetings: {e}");
                        return glib::ExitCode::FAILURE;
                    }
                    saved = next;
                }
                if let Some(m) = detected {
                    match start(m.title.as_deref()) {
                        Ok(StartOutcome::Started) => {
                            eprintln!("Automatic recording started ({})", m.provider)
                        }
                        Ok(StartOutcome::Busy) => eprintln!(
                            "Automatic recording: Recorder is busy; leaving the existing recording unchanged"
                        ),
                        Ok(StartOutcome::LaunchFailed(e)) => {
                            // No recording command was sent. Retry a failed launch
                            // after a short cooldown; handled calls have no timeout.
                            seen.launch_failed(&m.key, ipc::now());
                            saved = seen.saved(&session);
                            if let Err(error) = save_seen(&saved) {
                                eprintln!("Could not save launch retry: {error}");
                                return glib::ExitCode::FAILURE;
                            }
                            eprintln!("Automatic recording: {e}; retrying in 30 seconds");
                        }
                        Err(e) => eprintln!("Automatic recording: {e}"),
                    }
                }
            }
            Err(e) => {
                if e != last_error {
                    eprintln!("{e}");
                    last_error = e;
                }
                // A failed query does not mean the meeting window closed.
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

/// Use captured audio duration, so paused time does not count toward the minute.
pub fn skip_short_transcription(auto_start_enabled: bool, duration_secs: i64) -> bool {
    auto_start_enabled && duration_secs < 60
}

pub fn enabled() -> bool {
    systemctl(&["is-enabled", "--quiet", SERVICE]).is_ok()
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    if !enabled {
        return systemctl(&["disable", "--now", SERVICE]);
    }
    if glib::find_program_in_path("hyprctl").is_none() {
        return Err("Automatic recording requires Hyprland".into());
    }
    let exe = executable()?;
    let path = exe
        .to_str()
        .ok_or("The application path is not valid UTF-8")?;
    if path.chars().any(char::is_control) {
        return Err("Invalid application path".into());
    }
    let escaped = path
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    let dir = glib::user_config_dir().join("systemd/user");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(SERVICE), format!(
        "[Unit]\nDescription=Automatically record meeting windows\nPartOf=graphical-session.target\nAfter=graphical-session.target\n\n[Service]\nExecStart=\"{escaped}\" auto-record\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=graphical-session.target\n"
    )).map_err(|e| e.to_string())?;
    // Capture the active compositor/session for an already-running user manager.
    systemctl(&[
        "import-environment",
        "HYPRLAND_INSTANCE_SIGNATURE",
        "WAYLAND_DISPLAY",
        "DISPLAY",
    ])?;
    systemctl(&["daemon-reload"])?;
    if let Err(e) = systemctl(&["enable", "--now", SERVICE]) {
        let _ = systemctl(&["disable", "--now", SERVICE]);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn web(code: &str) -> Value {
        serde_json::json!({"address":"0x1","class":"chromium","title":format!("Meet - {code} - Chromium")})
    }
    #[test]
    fn short_recordings_skip_transcription_only_with_auto_start_enabled() {
        for seconds in [0, 1, 59] {
            assert!(skip_short_transcription(true, seconds));
            assert!(!skip_short_transcription(false, seconds));
        }
        for seconds in [60, 61, 7200] {
            assert!(!skip_short_transcription(true, seconds));
            assert!(!skip_short_transcription(false, seconds));
        }
    }

    #[test]
    fn replaced_executable_uses_installed_path() {
        let real = std::env::current_exe().unwrap();
        assert_eq!(installed_executable(real.clone()).unwrap(), real);
        let mut deleted = real.clone().into_os_string();
        deleted.push(" (deleted)");
        assert_eq!(installed_executable(deleted.into()).unwrap(), real);
        assert!(installed_executable(real.join("missing (deleted)")).is_err());
    }
    #[test]
    fn failed_launch_retries_after_cooldown_and_restart() {
        let mut seen = Seen::default();
        let clients = [web("Team sync")];
        seen.update_at(&clients, 100);
        let meeting = seen.update_at(&clients, 102).unwrap();
        seen.launch_failed(&meeting.key, 103);
        let mut restored = Seen::restore_at(&seen.saved("session"), "session", 104);
        assert!(restored.update_at(&clients, 132).is_none());
        assert!(restored.update_at(&clients, 133).is_none());
        assert!(restored.update_at(&clients, 135).is_some());
    }
    #[test]
    fn starts_a_stable_meeting_once_even_after_tab_switch_or_restart() {
        let mut seen = Seen::default();
        let m = web("abc-defg-hij");
        assert!(seen.update(&[m.clone()]).is_none());
        assert!(seen.update(&[m.clone()]).is_some());
        let mut other_tab = m.clone();
        other_tab["title"] = "Inbox - Chromium".into();
        assert!(seen.update(&[other_tab]).is_none());
        let mut seen = Seen::restore(&seen.saved("session"), "session");
        assert!(seen.update(&[m.clone()]).is_none());
        assert!(seen.update(&[m]).is_none());
    }
    #[test]
    fn another_meet_code_or_reopened_window_can_start() {
        let mut seen = Seen::default();
        seen.update(&[web("abc-defg-hij")]);
        seen.update(&[web("abc-defg-hij")]);
        assert!(seen.update(&[web("klm-nopq-rst")]).is_none());
        assert!(seen.update(&[web("klm-nopq-rst")]).is_some());
        seen.update(&[]);
        assert!(seen.update(&[web("klm-nopq-rst")]).is_none());
        assert!(seen.update(&[web("klm-nopq-rst")]).is_some());
    }
    #[test]
    fn different_named_meetings_in_one_browser_window_can_start() {
        let mut seen = Seen::default();
        seen.update(&[web("First planning call")]);
        assert!(seen.update(&[web("First planning call")]).is_some());
        let mut seen = Seen::restore(&seen.saved("session"), "session");
        assert!(seen.update(&[web("Next planning call")]).is_none());
        assert!(seen.update(&[web("Next planning call")]).is_some());
        assert!(seen.update(&[web("Next planning call")]).is_none());
        // Switching back to a previously handled call must not restart it.
        assert!(seen.update(&[web("First planning call")]).is_none());
    }

    #[test]
    fn long_calls_stay_suppressed_across_restarts_and_tab_switches() {
        let clients = [web("Daily meeting")];
        let mut seen = Seen::default();
        assert!(seen.update_at(&clients, 100).is_none());
        assert!(seen.update_at(&clients, 102).is_some());
        for now in [7302, 7304, 24 * 60 * 60, 7 * 24 * 60 * 60] {
            assert!(seen.update_at(&clients, now).is_none());
            let saved = seen.saved("session");
            seen = Seen::restore_at(&saved, "session", now + 1);
            assert!(seen.update_at(&clients, now + 2).is_none());
            let mut tab = clients[0].clone();
            tab["title"] = "Inbox - Chromium".into();
            assert!(seen.update_at(&[tab], now + 3).is_none());
            assert!(seen.update_at(&clients, now + 4).is_none());
            assert_eq!(seen.saved("session"), saved);
        }
        // Closing the meeting window, unlike closing the recorder, releases it.
        seen.update_at(&[], 8 * 24 * 60 * 60);
        assert!(seen.update_at(&clients, 8 * 24 * 60 * 60 + 2).is_none());
        assert!(seen.update_at(&clients, 8 * 24 * 60 * 60 + 4).is_some());
    }

    #[test]
    fn old_deadlines_migrate_without_restarting_a_long_call() {
        let client = web("Daily meeting");
        let key = meeting_detection::detect(&client).unwrap().key;
        for entry in [
            serde_json::json!("0x1"),
            serde_json::json!({"window":"0x1", "expires_at":200}),
            serde_json::json!({"window":"0x1", "expires_at":10000}),
        ] {
            let old = serde_json::json!({"session":"session", "handled":{key.clone():entry}});
            let mut seen = Seen::restore_at(&old.to_string(), "session", 8000);
            assert_eq!(seen.handled.len(), 1);
            assert_eq!(seen.handled[&key].retry_after, None);
            assert!(seen.update_at(&[client.clone()], 8002).is_none());
            let mut restored = Seen::restore_at(&seen.saved("session"), "session", 20000);
            assert!(restored.update_at(&[client.clone()], 20002).is_none());
            assert!(restored.update_at(&[], 20004).is_none());
            assert!(restored.handled.is_empty());
        }
        let invalid = r#"{"session":"session","handled":{"bad":{"window":"0x1","expires_at":"invalid"},"bad_retry":{"window":"0x1","retry_after":"invalid"}}}"#;
        assert!(Seen::restore_at(invalid, "session", 100).handled.is_empty());
    }

    #[test]
    fn same_title_rooms_are_distinct_and_renames_do_not_restart() {
        for (first, second) in [
            (
                "chrome-meet.google.com__abc-defg-hij-Default",
                "chrome-meet.google.com__klm-nopq-rst-Default",
            ),
            (
                "chrome-app.zoom.us__wc_join_12345678901-Default",
                "chrome-app.zoom.us__wc_join_98765432109-Default",
            ),
        ] {
            let mut client =
                serde_json::json!({"address":"0x1", "class":first, "title":"Daily sync"});
            let mut seen = Seen::default();
            assert!(seen.update_at(&[client.clone()], 100).is_none());
            assert!(seen.update_at(&[client.clone()], 102).is_some());
            client["title"] = "Renamed sync".into();
            assert!(seen.update_at(&[client.clone()], 104).is_none());
            client["title"] = "Daily sync".into();
            client["class"] = second.into();
            assert!(seen.update_at(&[client.clone()], 106).is_none());
            assert!(seen.update_at(&[client], 108).is_some());
        }
    }

    #[test]
    fn upgrading_fallback_suppression_preserves_retry_deadline() {
        let client = web("abc-defg-hij");
        let detected = meeting_detection::detect(&client).unwrap();
        let mut seen = Seen::default();
        seen.handled.insert(
            detected.fallback_key.clone(),
            Handled {
                window: detected.window,
                retry_after: Some(200),
            },
        );
        assert!(seen.update_at(&[client], 100).is_none());
        assert!(!seen.handled.contains_key(&detected.fallback_key));
        assert_eq!(seen.handled[&detected.key].retry_after, Some(200));
    }

    #[test]
    fn ambiguous_windows_never_auto_start() {
        let mut a = web("abc-defg-hij");
        let b = web("klm-nopq-rst");
        a["address"] = "0x2".into();
        let mut seen = Seen::default();
        for _ in 0..3 {
            assert!(seen.update(&[a.clone(), b.clone()]).is_none());
        }
    }
    #[test]
    fn new_session_does_not_inherit_window_addresses() {
        let mut seen = Seen::default();
        seen.update(&[web("abc-defg-hij")]);
        seen.update(&[web("abc-defg-hij")]);
        assert!(Seen::restore(&seen.saved("old"), "new").handled.is_empty());
        assert!(Seen::restore("broken JSON", "new").handled.is_empty());
    }
}
