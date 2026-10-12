//! Conservative end hints: an unrelated selected browser tab is not a leave.
use serde_json::Value;
use std::time::{Duration, Instant};

pub struct Watch {
    window: String,
    class: String,
    ended_since: Option<Instant>,
}
impl Watch {
    pub fn bind(clients: &[Value]) -> Option<Self> {
        let meeting = crate::meeting_detection::unique(clients)?;
        let client = clients
            .iter()
            .find(|c| c["address"].as_str() == Some(&meeting.window))?;
        Some(Self {
            window: meeting.window,
            class: client["class"].as_str()?.into(),
            ended_since: None,
        })
    }
    pub fn observe(&mut self, clients: Option<&[Value]>, now: Instant) -> bool {
        let Some(clients) = clients else {
            self.ended_since = None;
            return false;
        };
        let ended = match clients
            .iter()
            .find(|c| c["address"].as_str() == Some(&self.window))
        {
            None => true,
            Some(client) => {
                let class = client["class"].as_str().unwrap_or("");
                let title = client["title"].as_str().unwrap_or("").to_ascii_lowercase();
                class == self.class
                    && (matches!(
                        title.as_str(),
                        "you left the meeting" | "you've left the meeting" | "meeting has ended"
                    ) || matches!(
                        title.as_str(),
                        "meet - you left the meeting - chromium"
                            | "meet - you've left the meeting - chromium"
                    ) || (matches!(class, "zoom" | "Zoom" | "zoom.real")
                        && matches!(title.as_str(), "zoom" | "zoom workplace")))
            }
        };
        if !ended {
            self.ended_since = None;
            return false;
        }
        now.duration_since(*self.ended_since.get_or_insert(now)) >= Duration::from_secs(30)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn client(title: &str) -> Value {
        serde_json::json!({"address":"0x1", "class":"chromium", "title":title})
    }
    #[test]
    fn closed_window_requires_grace_and_reconnection_resets_it() {
        let call = [client("Meet - Team sync - Chromium")];
        let mut watch = Watch::bind(&call).unwrap();
        let now = Instant::now();
        assert!(!watch.observe(Some(&[]), now));
        assert!(!watch.observe(Some(&[]), now + Duration::from_secs(29)));
        assert!(!watch.observe(Some(&call), now + Duration::from_secs(30)));
        assert!(!watch.observe(Some(&[]), now + Duration::from_secs(31)));
        assert!(watch.observe(Some(&[]), now + Duration::from_secs(61)));
    }
    #[test]
    fn tab_switches_unchanged_titles_and_query_failures_do_not_stop() {
        let call = [client("Meet - Team sync - Chromium")];
        let mut watch = Watch::bind(&call).unwrap();
        let now = Instant::now();
        for title in ["Mail - Chromium", "Meet - Team sync - Chromium"] {
            assert!(!watch.observe(Some(&[client(title)]), now));
            assert!(!watch.observe(Some(&[client(title)]), now + Duration::from_secs(60)));
        }
        assert!(!watch.observe(Some(&[]), now));
        assert!(!watch.observe(None, now + Duration::from_secs(31)));
        assert!(!watch.observe(Some(&[]), now + Duration::from_secs(32)));
    }
    #[test]
    fn native_zoom_home_ends_only_the_associated_window() {
        let call = serde_json::json!({"address":"0x1", "class":"zoom", "title":"Zoom Meeting"});
        let mut watch = Watch::bind(&[call.clone()]).unwrap();
        let now = Instant::now();
        let home = serde_json::json!({"address":"0x1", "class":"zoom", "title":"Zoom Workplace"});
        assert!(!watch.observe(Some(&[home.clone()]), now));
        assert!(watch.observe(Some(&[home]), now + Duration::from_secs(30)));
        let mut watch = Watch::bind(&[call.clone()]).unwrap();
        let other_home =
            serde_json::json!({"address":"0x2", "class":"zoom", "title":"Zoom Workplace"});
        assert!(!watch.observe(Some(&[call.clone(), other_home.clone()]), now));
        assert!(!watch.observe(Some(&[call, other_home]), now + Duration::from_secs(30)));
        assert!(Watch::bind(&[]).is_none());
    }

    #[test]
    fn explicit_leave_and_ambiguity() {
        let call = [client("Meet - Team sync - Chromium")];
        let mut watch = Watch::bind(&call).unwrap();
        let now = Instant::now();
        let ended = [client("You left the meeting")];
        assert!(!watch.observe(Some(&ended), now));
        assert!(watch.observe(Some(&ended), now + Duration::from_secs(30)));
        let mut second = call[0].clone();
        second["address"] = "0x2".into();
        assert!(Watch::bind(&[call[0].clone(), second]).is_none());
    }
}
