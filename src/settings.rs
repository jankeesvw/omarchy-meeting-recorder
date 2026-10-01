//! Remembered preferences: the audio format, the transcription language,
//! the microphone and output to record, the name you go by in transcripts and
//! whether the bar widget was offered.

use std::path::{Path, PathBuf};

use gtk::glib;

use crate::APP_NAME;
use crate::audio::{Device, Role};
use crate::export::Format;
use crate::transcribe::LANGUAGES;

fn path() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| glib::home_dir().join(".local/state"));
    state.join(APP_NAME).join("settings.json")
}

fn load() -> serde_json::Value {
    load_from(&path())
}

fn load_from(path: &Path) -> serde_json::Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Updates one key and keeps the others.
fn save(key: &str, value: &str) {
    save_to(&path(), &[(key.to_owned(), value.to_owned())]);
}

/// Updates `entries` at once and keeps the other keys.
fn save_to(path: &Path, entries: &[(String, String)]) {
    let mut settings = load_from(path);
    for (key, value) in entries {
        settings[key] = serde_json::Value::String(value.clone());
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, settings.to_string());
}

pub fn load_format() -> Format {
    load()["format"]
        .as_str()
        .map(Format::from_key)
        .unwrap_or(Format::Mono)
}

pub fn save_format(format: Format) {
    save("format", format.key());
}

/// A whisper language code from `LANGUAGES`, "auto" when unset or unknown.
pub fn load_language() -> &'static str {
    let settings = load();
    let saved = settings["language"].as_str().unwrap_or("auto");
    LANGUAGES
        .iter()
        .map(|(code, _)| *code)
        .find(|code| *code == saved)
        .unwrap_or("auto")
}

pub fn save_language(code: &str) {
    save("language", code);
}

/// The device picked to record `role`, `None` for the system default.
pub fn load_device(role: Role) -> Option<Device> {
    device_in(&load(), role)
}

/// What `role` records: the picked device, or else the system default.
pub fn device_to_record(role: Role) -> String {
    load_device(role).map_or_else(|| role.default_device().to_owned(), |d| d.name)
}

fn device_in(settings: &serde_json::Value, role: Role) -> Option<Device> {
    let key = device_key(role);
    let name = settings[format!("{key}_device")]
        .as_str()
        .filter(|n| !n.is_empty())?;
    let label = settings[format!("{key}_device_label")]
        .as_str()
        .unwrap_or(name);
    Some(Device {
        name: name.to_owned(),
        label: label.to_owned(),
    })
}

/// `None` goes back to the system default.
pub fn save_device(role: Role, device: Option<&Device>) {
    save_to(&path(), &device_entries(role, device));
}

fn device_entries(role: Role, device: Option<&Device>) -> [(String, String); 2] {
    let key = device_key(role);
    let (name, label) = device.map_or(("", ""), |d| (d.name.as_str(), d.label.as_str()));
    [
        (format!("{key}_device"), name.to_owned()),
        (format!("{key}_device_label"), label.to_owned()),
    ]
}

fn device_key(role: Role) -> &'static str {
    match role {
        Role::Mic => "mic",
        Role::System => "system",
    }
}

/// What the mic side is called in new transcripts, "You" until you change it.
pub fn load_your_name() -> String {
    load()["your_name"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(crate::meeting::DEFAULT_YOU)
        .to_owned()
}

pub fn save_your_name(name: &str) {
    save("your_name", name);
}

/// Whether the app already asked to put its widget in the bar.
pub fn bar_widget_offered() -> bool {
    load()["bar_widget_offered"].as_str() == Some("yes")
}

pub fn set_bar_widget_offered() {
    save("bar_widget_offered", "yes");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, label: &str) -> Device {
        Device {
            name: name.to_owned(),
            label: label.to_owned(),
        }
    }

    #[test]
    fn device_round_trip() {
        let dir = std::env::temp_dir().join(format!("mr-settings-{}", std::process::id()));
        let path = dir.join("settings.json");
        save_to(&path, &[("language".to_owned(), "de".to_owned())]);
        assert_eq!(device_in(&load_from(&path), Role::Mic), None);

        let headset = device("alsa_input.usb-headset", "Headset");
        save_to(&path, &device_entries(Role::Mic, Some(&headset)));
        let settings = load_from(&path);
        assert_eq!(device_in(&settings, Role::Mic), Some(headset));
        assert_eq!(device_in(&settings, Role::System), None);
        assert_eq!(settings["language"], "de");

        // Back to the system default.
        save_to(&path, &device_entries(Role::Mic, None));
        assert_eq!(device_in(&load_from(&path), Role::Mic), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn device_without_a_label() {
        let settings = serde_json::json!({"system_device": "alsa_output.x.monitor"});
        assert_eq!(
            device_in(&settings, Role::System),
            Some(device("alsa_output.x.monitor", "alsa_output.x.monitor"))
        );
    }
}
