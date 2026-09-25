//! Optional room identifiers. Never persist invitation credentials.
use gtk::glib;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

#[derive(Clone, Debug, PartialEq)]
pub struct Room {
    pub provider: String,
    pub id: String,
}

pub fn parse(link: &str) -> Result<Room, String> {
    let invalid = || "Enter a Google Meet room link or a Zoom numeric meeting link.".to_owned();
    let uri = glib::Uri::parse(link.trim(), glib::UriFlags::NONE).map_err(|_| invalid())?;
    if uri.scheme() != "https" || uri.userinfo().is_some() || !matches!(uri.port(), -1 | 443) {
        return Err(invalid());
    }
    let host = uri.host().ok_or_else(invalid)?.to_ascii_lowercase();
    let path = uri.path();
    let parts: Vec<_> = path.trim_matches('/').split('/').collect();
    if host == "meet.google.com"
        && parts.len() == 1
        && crate::meeting_detection::meet_code(parts[0])
    {
        return Ok(Room {
            provider: "Google Meet".into(),
            id: parts[0].into(),
        });
    }
    if host == "zoom.us" || host.ends_with(".zoom.us") {
        let id = match parts.as_slice() {
            ["j", id] | ["wc", "join", id] | ["wc", id, "join"] => Some(*id),
            _ => None,
        };
        if let Some(id) = id
            && (9..=11).contains(&id.len())
            && id.bytes().all(|b| b.is_ascii_digit())
        {
            return Ok(Room {
                provider: "Zoom".into(),
                id: id.into(),
            });
        }
    }
    Err(invalid())
}

pub fn from_class(class: &str) -> Option<Room> {
    let (host, path) = class.strip_prefix("chrome-")?.split_once("__")?;
    // The profile suffix follows the launch URL. Meet codes themselves contain hyphens.
    let path = if host == "meet.google.com" {
        if !path.get(12..)?.starts_with('-') {
            return None;
        }
        path.get(..12)?
    } else {
        path.split('-').next()?
    };
    parse(&format!("https://{host}/{}", path.replace('_', "/"))).ok()
}

struct Hint {
    session: String,
    fallback: String,
    expires: i64,
    room: Room,
}
impl Hint {
    fn json(&self) -> serde_json::Value {
        serde_json::json!({"session": self.session, "fallback": self.fallback, "expires": self.expires, "provider": self.room.provider, "id": self.room.id})
    }
    fn from_json(v: &serde_json::Value) -> Option<Self> {
        Some(Self {
            session: v["session"].as_str()?.into(),
            fallback: v["fallback"].as_str()?.into(),
            expires: v["expires"].as_i64()?,
            room: Room {
                provider: v["provider"].as_str()?.into(),
                id: v["id"].as_str()?.into(),
            },
        })
    }
}
fn path() -> std::path::PathBuf {
    glib::user_runtime_dir().join("omarchy-meeting-recorder-room.json")
}

pub fn apply(link: &str) -> Result<(), String> {
    let room = parse(link)?;
    let clients = crate::meeting_detection::clients()?;
    let meeting = crate::meeting_detection::unique(&clients)
        .ok_or("Open exactly one meeting window before applying its link.")?;
    if meeting.provider != room.provider {
        return Err("The link must match the detected meeting provider.".into());
    }
    if meeting
        .room
        .as_ref()
        .is_some_and(|detected| detected != &room)
    {
        return Err("The link does not match the room ID exposed by the meeting window.".into());
    }
    let hint = Hint {
        session: std::env::var("HYPRLAND_INSTANCE_SIGNATURE").map_err(|_| "No Hyprland session")?,
        fallback: meeting.fallback_key,
        expires: crate::ipc::now().saturating_add(7200),
        room,
    };
    let temporary = path().with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|e| e.to_string())?;
    file.write_all(hint.json().to_string().as_bytes())
        .map_err(|e| e.to_string())?;
    std::fs::rename(temporary, path()).map_err(|e| e.to_string())
}

pub fn augment(meeting: &mut crate::meeting_detection::DetectedMeeting) {
    if meeting.room.is_some() {
        return;
    }
    let Some(hint) = std::fs::read(path()).ok().and_then(|data| {
        serde_json::from_slice::<serde_json::Value>(&data)
            .ok()
            .and_then(|v| Hint::from_json(&v))
    }) else {
        return;
    };
    if valid_hint(
        &hint,
        meeting,
        &std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap_or_default(),
        crate::ipc::now(),
    ) {
        meeting.set_room(hint.room);
    }
}
fn valid_hint(
    hint: &Hint,
    meeting: &crate::meeting_detection::DetectedMeeting,
    session: &str,
    now: i64,
) -> bool {
    !session.is_empty()
        && hint.session == session
        && hint.expires > now
        && hint.fallback == meeting.fallback_key
        && hint.room.provider == meeting.provider
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn links_drop_credentials_and_distinguish_rooms() {
        for url in [
            "https://zoom.us/j/12345678901?pwd=secret",
            "https://us02web.zoom.us/j/12345678901",
            "https://app.zoom.us/wc/join/12345678901",
            "https://app.zoom.us/wc/12345678901/join",
        ] {
            assert_eq!(parse(url).unwrap().id, "12345678901");
            assert!(!format!("{:?}", parse(url).unwrap()).contains("secret"));
        }
        assert_eq!(
            parse("https://meet.google.com/abc-defg-hij?authuser=1")
                .unwrap()
                .id,
            "abc-defg-hij"
        );
        for url in [
            "https://zoom.us.evil.org/j/12345678901",
            "https://evilzoom.us/j/12345678901",
            "https://user@zoom.us/j/12345678901",
            "http://zoom.us/j/12345678901",
            "https://zoom.us/my/person",
            "https://meet.google.com/lookup/name",
        ] {
            assert!(parse(url).is_err(), "{url}");
        }
    }
    #[test]
    fn web_app_launch_ids() {
        assert_eq!(
            from_class("chrome-meet.google.com__abc-defg-hij-Default")
                .unwrap()
                .id,
            "abc-defg-hij"
        );
        assert_eq!(
            from_class("chrome-app.zoom.us__wc_join_12345678901-Default")
                .unwrap()
                .id,
            "12345678901"
        );
        assert!(from_class("chrome-meet.google.com__-Default").is_none());
    }
    #[test]
    fn hints_are_scoped_and_expire() {
        let meeting = crate::meeting_detection::detect(
            &serde_json::json!({"address":"0x1", "class":"zoom", "title":"Zoom Meeting"}),
        )
        .unwrap();
        let hint = Hint {
            session: "session".into(),
            fallback: meeting.fallback_key.clone(),
            expires: 100,
            room: parse("https://zoom.us/j/12345678901").unwrap(),
        };
        assert!(valid_hint(&hint, &meeting, "session", 99));
        assert!(!valid_hint(&hint, &meeting, "session", 100));
        assert!(!valid_hint(&hint, &meeting, "another-session", 99));
        let mut other = meeting.clone();
        other.fallback_key.push('x');
        assert!(!valid_hint(&hint, &other, "session", 99));
    }
}
