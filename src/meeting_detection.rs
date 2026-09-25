//! Meeting-window hints from Hyprland. These are not proof that a call was
//! joined: browser titles can also be present on a pre-join screen.
use std::process::Command;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct DetectedMeeting {
    pub key: String,
    pub fallback_key: String,
    pub room: Option<crate::room_identity::Room>,
    pub window: String,
    pub provider: &'static str,
    pub title: Option<String>,
}

fn browser_title(title: &str) -> &str {
    [
        " - Google Chrome",
        " - Chromium",
        " - Brave",
        " - Microsoft Edge",
        " — Mozilla Firefox",
        " — Firefox",
    ]
    .into_iter()
    .find_map(|suffix| title.strip_suffix(suffix))
    .unwrap_or(title)
    .trim()
}

fn browser(class: &str) -> bool {
    matches!(
        class.to_ascii_lowercase().as_str(),
        "chromium"
            | "google-chrome"
            | "google-chrome-stable"
            | "brave-browser"
            | "brave"
            | "firefox"
            | "org.mozilla.firefox"
            | "microsoft-edge"
    )
}

pub(crate) fn meet_code(text: &str) -> bool {
    text.len() == 12
        && text.bytes().enumerate().all(|(i, c)| {
            if i == 3 || i == 8 {
                c == b'-'
            } else {
                c.is_ascii_lowercase()
            }
        })
}

fn generic(title: &str) -> bool {
    let title = title.to_ascii_lowercase();
    matches!(
        title.as_str(),
        "" | "zoom"
            | "zoom workplace"
            | "zoom meetings"
            | "zoom web client"
            | "google meet"
            | "meet"
            | "meet -"
            | "join meeting"
            | "join a meeting"
            | "join a meeting - zoom"
            | "launch meeting - zoom"
            | "loading…"
            | "loading..."
    ) || title.starts_with("http://")
        || title.starts_with("https://")
        || title.starts_with("app.zoom.us")
        || title.starts_with("meet.google.com")
        || title.contains("waiting for the host")
        || title.contains("waiting room")
        || title.contains("ready to join")
        || title.contains("you left the meeting")
        || title.contains("you've left the meeting")
        || title.contains("meeting has ended")
        || title.starts_with("sign in")
}

pub fn detect(client: &Value) -> Option<DetectedMeeting> {
    let class = client["class"].as_str()?;
    let window = client["address"].as_str()?;
    let title = browser_title(client["title"].as_str()?.trim());
    if window.is_empty() || title.chars().any(char::is_control) || generic(title) {
        return None;
    }
    let class_room = crate::room_identity::from_class(class);
    let zoom_web = class_room.as_ref().is_some_and(|r| r.provider == "Zoom")
        || class
            .strip_prefix("chrome-app.zoom.us__wc_join_")
            .or_else(|| class.strip_prefix("chrome-app.zoom.us__wc_"))
            .is_some_and(|suffix| suffix.starts_with(|c: char| c.is_ascii_digit()));
    let zoom_native = matches!(class, "zoom" | "Zoom" | "zoom.real")
        && (title == "Zoom Meeting" || title.ends_with(" - Zoom Meeting"));
    let (provider, name, identity) = if zoom_web || zoom_native {
        (
            "Zoom",
            if title == "Zoom Meeting" {
                None
            } else {
                Some(title.strip_suffix(" - Zoom Meeting").unwrap_or(title))
            },
            class,
        )
    } else {
        let dedicated = class.starts_with("chrome-meet.google.com__");
        let marked = title
            .strip_prefix("Meet - ")
            .or_else(|| title.strip_suffix(" - Google Meet"));
        if !dedicated && !(browser(class) && marked.is_some()) {
            return None;
        }
        let name = marked.unwrap_or(title).trim();
        if generic(name) {
            return None;
        }
        // Use the visible code or name as the identity, not the browser class:
        // unrelated named meetings often reuse the same browser window. A code
        // is not a meeting name, so do not fill the name field with it.
        (
            "Google Meet",
            if meet_code(name) { None } else { Some(name) },
            name,
        )
    };
    let title = name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(200).collect());
    let fallback_key = format!("{window}:{provider}:{identity}");
    let mut meeting = DetectedMeeting {
        key: fallback_key.clone(),
        fallback_key,
        room: None,
        window: window.into(),
        provider,
        title,
    };
    let room = if provider == "Google Meet" && meet_code(identity) {
        Some(crate::room_identity::Room {
            provider: provider.into(),
            id: identity.into(),
        })
    } else {
        class_room
    };
    if let Some(room) = room {
        meeting.set_room(room);
    }
    Some(meeting)
}

impl DetectedMeeting {
    pub fn set_room(&mut self, room: crate::room_identity::Room) {
        self.key = format!("{}:{}:room:{}", self.window, self.provider, room.id);
        self.room = Some(room);
    }
}

pub fn clients() -> Result<Vec<Value>, String> {
    let output = Command::new("hyprctl")
        .args(["-j", "clients"])
        .output()
        .map_err(|e| format!("Could not query Hyprland: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "hyprctl: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("Invalid Hyprland window list: {e}"))
}

/// Never guess which meeting to name when several meeting windows are open.
pub fn unique(clients: &[Value]) -> Option<DetectedMeeting> {
    let mut detected = clients.iter().filter_map(detect);
    let first = detected.next()?;
    if detected.next().is_some() {
        None
    } else {
        Some(first)
    }
}

/// A manually entered name takes precedence. An unchanged automatic suggestion
/// can follow the detected meeting, or be cleared when the meeting disappears.
pub fn suggested_title(
    current: &str,
    previous: Option<&str>,
    detected: Option<&str>,
) -> Option<String> {
    if !current.is_empty() && Some(current) != previous {
        return None;
    }
    let next = detected.unwrap_or("");
    (current != next).then(|| next.to_owned())
}

pub fn check() -> gtk::glib::ExitCode {
    match clients() {
        Ok(clients) => {
            let meetings: Vec<_> = clients
                .iter()
                .filter_map(detect)
                .map(|m| serde_json::json!({"provider": m.provider, "title": m.title}))
                .collect();
            println!("{}", serde_json::json!(meetings));
            gtk::glib::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            gtk::glib::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn client(class: &str, title: &str) -> Value {
        serde_json::json!({"address":"0x1", "class":class, "title":title})
    }
    #[test]
    fn zoom_topic_and_native_fallback() {
        assert_eq!(
            detect(&client(
                "chrome-app.zoom.us__wc_join_123-Default",
                "Team Sync"
            ))
            .unwrap()
            .title
            .as_deref(),
            Some("Team Sync")
        );
        assert_eq!(
            detect(&client("zoom", "Planning - Zoom Meeting"))
                .unwrap()
                .title
                .as_deref(),
            Some("Planning")
        );
        assert_eq!(detect(&client("zoom", "Zoom Meeting")).unwrap().title, None);
        assert!(detect(&client("chromium", "Zoom docs")).is_none());
    }
    #[test]
    fn google_meet_web_app_and_visible_browser_tab() {
        for (class, title, expected) in [
            (
                "chrome-meet.google.com__-Default",
                "Design review",
                Some("Design review"),
            ),
            ("chromium", "Meet - abc-defg-hij - Chromium", None),
            (
                "google-chrome",
                "Design review - Google Meet - Google Chrome",
                Some("Design review"),
            ),
            ("firefox", "Meet - abc-defg-hij — Mozilla Firefox", None),
        ] {
            let m = detect(&client(class, title)).unwrap();
            assert_eq!(m.provider, "Google Meet");
            assert_eq!(m.title.as_deref(), expected);
        }
        assert!(detect(&client("terminal", "Meet - abc-defg-hij")).is_none());
    }
    #[test]
    fn home_join_waiting_and_ended_screens_are_ignored() {
        for title in [
            "",
            "Zoom",
            "Zoom Workplace",
            "Join a Meeting",
            "Waiting for the host",
            "Ready to join?",
            "You left the meeting",
            "Google Meet",
            "Meet",
            "Sign in - Google Meet",
            "meet.google.com/abc-defg-hij",
            "Meet - ",
            "Bad\nTitle",
        ] {
            for class in [
                "chrome-meet.google.com__-Default",
                "chrome-app.zoom.us__wc_join_123-Default",
            ] {
                assert!(detect(&client(class, title)).is_none(), "{class}: {title}");
            }
        }
    }
    #[test]
    fn codes_and_ambiguous_windows_are_not_used_as_names() {
        assert!(meet_code("abc-defg-hij"));
        assert!(!meet_code("abc-def-hijk"));
        let a = client("chromium", "Meet - abc-defg-hij");
        let b = client("zoom", "Zoom Meeting");
        assert!(unique(&[a.clone(), b]).is_none());
        assert!(unique(&[a]).is_some());
    }
    #[test]
    fn current_meet_code_overrides_launch_room() {
        let meeting = detect(&client(
            "chrome-meet.google.com__abc-defg-hij-Default",
            "Meet - klm-nopq-rst",
        ))
        .unwrap();
        assert_eq!(meeting.room.unwrap().id, "klm-nopq-rst");
    }
    #[test]
    fn manual_titles_win_and_stale_suggestions_clear() {
        assert_eq!(suggested_title("", None, Some("Call")), Some("Call".into()));
        assert_eq!(suggested_title("My name", None, Some("Call")), None);
        assert_eq!(
            suggested_title("Edited name", Some("Call"), Some("Next")),
            None
        );
        assert_eq!(
            suggested_title("Call", Some("Call"), Some("Next")),
            Some("Next".into())
        );
        assert_eq!(suggested_title("Call", Some("Call"), None), Some("".into()));
    }
}
