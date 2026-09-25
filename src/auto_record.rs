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
const SUPPRESSION_SECS: i64 = 2 * 60 * 60;

struct Handled {
    window: String,
    expires_at: i64,
}

#[derive(Default)]
struct Seen {
    pending: Option<(String, Option<String>, usize)>,
    // Suppress repeats for two hours, including tab switches and watcher
    // restarts. Closing a window releases its entries sooner.
    handled: HashMap<String, Handled>,
}

impl Seen {
    fn update(&mut self, clients: &[Value]) -> Option<DetectedMeeting> {
        self.update_at(clients, ipc::now())
    }

    fn update_at(&mut self, clients: &[Value], now: i64) -> Option<DetectedMeeting> {
        self.handled.retain(|_, entry| {
            entry.expires_at > now
                && clients
                    .iter()
                    .any(|c| c["address"].as_str() == Some(entry.window.as_str()))
        });
        let Some(mut m) = meeting_detection::unique(clients) else {
            self.pending = None;
            return None;
        };
        crate::room_identity::augment(&mut m);
        // Upgrade an old fallback suppression once, without extending its deadline.
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
                expires_at: now.saturating_add(SUPPRESSION_SECS),
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
                // Migrate the old untimed format once, without immediately
                // restarting a meeting after upgrading the watcher.
                let entry = if let Some(window) = value.as_str() {
                    Some((window, now.saturating_add(SUPPRESSION_SECS)))
                } else {
                    value["window"].as_str().zip(value["expires_at"].as_i64())
                };
                if let Some((window, expires_at)) = entry
                    && expires_at > now
                {
                    seen.handled.insert(
                        key.clone(),
                        Handled {
                            window: window.into(),
                            expires_at: expires_at.min(now.saturating_add(SUPPRESSION_SECS)),
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
                    serde_json::json!({"window":entry.window,"expires_at":entry.expires_at}),
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

fn start(title: Option<&str>) -> Result<(), String> {
    let mut status = ipc::snapshot();
    if status.is_err() {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
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
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "Could not open the recorder: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
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
        return Err("Recorder is busy; leaving the existing recording unchanged".into());
    }
    if !ipc::auto_start(title) {
        return Err("Could not send the recording command".into());
    }
    for _ in 0..5 {
        let status = ipc::snapshot()?;
        if matches!(status["state"].as_str(), Some("recording" | "paused")) {
            return Ok(());
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
    // Persist migrations/expired entries on the first successful window query.
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
                        Ok(()) => eprintln!("Automatic recording started ({})", m.provider),
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
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
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
    fn suppression_expires_after_two_hours_without_sliding_on_observation() {
        let mut seen = Seen::default();
        let clients = [web("Daily meeting")];
        assert!(seen.update_at(&clients, 100).is_none());
        assert!(seen.update_at(&clients, 102).is_some());
        assert!(
            seen.update_at(&clients, 102 + SUPPRESSION_SECS - 1)
                .is_none()
        );
        // Expiry still requires two stable observations before another attempt.
        assert!(seen.update_at(&clients, 102 + SUPPRESSION_SECS).is_none());
        assert!(seen.update_at(&clients, 104 + SUPPRESSION_SECS).is_some());
        assert!(seen.update_at(&clients, 106 + SUPPRESSION_SECS).is_none());
    }

    #[test]
    fn restarts_preserve_expiry_and_drop_expired_entries() {
        let clients = [web("Daily meeting")];
        let mut seen = Seen::default();
        seen.update_at(&clients, 100);
        seen.update_at(&clients, 102);
        let saved = seen.saved("session");
        let mut restored = Seen::restore_at(&saved, "session", 200);
        assert!(
            restored
                .update_at(&clients, 102 + SUPPRESSION_SECS - 1)
                .is_none()
        );
        assert!(
            restored
                .update_at(&clients, 102 + SUPPRESSION_SECS)
                .is_none()
        );
        assert!(
            restored
                .update_at(&clients, 104 + SUPPRESSION_SECS)
                .is_some()
        );
        assert!(
            Seen::restore_at(&saved, "session", 102 + SUPPRESSION_SECS)
                .handled
                .is_empty()
        );
    }

    #[test]
    fn old_state_migrates_once_and_invalid_expiries_are_ignored() {
        let old = r#"{"session":"session","handled":{"call":"0x1","bad":{"window":"0x1","expires_at":"invalid"}}}"#;
        let seen = Seen::restore_at(old, "session", 100);
        assert_eq!(seen.handled.len(), 1);
        assert_eq!(seen.handled["call"].expires_at, 100 + SUPPRESSION_SECS);
        let restored = Seen::restore_at(&seen.saved("session"), "session", 200);
        assert_eq!(restored.handled["call"].expires_at, 100 + SUPPRESSION_SECS);
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
    fn upgrading_fallback_suppression_preserves_deadline() {
        let client = web("abc-defg-hij");
        let detected = meeting_detection::detect(&client).unwrap();
        let mut seen = Seen::default();
        seen.handled.insert(
            detected.fallback_key.clone(),
            Handled {
                window: detected.window,
                expires_at: 200,
            },
        );
        assert!(seen.update_at(&[client], 100).is_none());
        assert!(!seen.handled.contains_key(&detected.fallback_key));
        assert_eq!(seen.handled[&detected.key].expires_at, 200);
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
