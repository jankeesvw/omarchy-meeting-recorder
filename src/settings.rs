//! Remembered preferences: the audio format, the transcription language,
//! the webcam, the inputs, the name you go by in transcripts and
//! whether the bar widget was offered.

use std::path::PathBuf;

use gtk::glib;

use crate::APP_NAME;
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
    std::fs::read_to_string(path())
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Updates one key and keeps the others.
fn save(key: &str, value: &str) {
    let mut settings = load();
    settings[key] = serde_json::Value::String(value.to_owned());
    let path = path();
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

/// The camera last recorded with, "" when there was none.
pub fn load_webcam() -> String {
    load()["webcam"].as_str().unwrap_or_default().to_owned()
}

pub fn save_webcam(name: &str) {
    save("webcam", name);
}

/// Whether the webcam toggle is on.
pub fn load_webcam_on() -> bool {
    load()["webcam_on"].as_str() == Some("yes")
}

pub fn save_webcam_on(on: bool) {
    save("webcam_on", if on { "yes" } else { "no" });
}

/// The microphones chosen to record, by PulseAudio source name: None until
/// a choice is made, which records the default microphone; empty when the
/// choice is no microphone at all.
pub fn load_mics() -> Option<Vec<String>> {
    let settings = load();
    let saved = settings["mics"].as_str()?;
    Some(
        saved
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

pub fn save_mics(names: &[String]) {
    save("mics", &names.join("\n"));
}

/// Whether the computer audio is recorded, as it is until switched off.
pub fn load_computer_audio() -> bool {
    load()["computer_audio"].as_str() != Some("off")
}

pub fn save_computer_audio(on: bool) {
    save("computer_audio", if on { "on" } else { "off" });
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
