//! Room identifiers extracted automatically from web-app window metadata.
use gtk::glib;

#[derive(Clone, Debug, PartialEq)]
pub struct Room {
    pub provider: String,
    pub id: String,
}

fn parse(link: &str) -> Result<Room, String> {
    let invalid = || "Unsupported meeting room URL".to_owned();
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
}
