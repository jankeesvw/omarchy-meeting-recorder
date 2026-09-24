//! Remembered preferences: the audio format, the transcription language,
//! the name you go by in transcripts and whether the bar widget was offered.

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

/// The user's own additions to the mishearing vocabulary: `{"Term": ["heard
/// as", ...]}`, merged onto the built-in list. Absent or malformed, this is
/// just an empty object; `vocabulary::Corrector` ignores malformed entries
/// inside it rather than giving up on the whole setting.
pub fn load_vocabulary() -> serde_json::Value {
    load()["vocabulary"].clone()
}
