//! Which whisper model transcribes, where it is on disk, and fetching it.
//!
//! The model is picked with `--model` on the command line, or `model = "…"`
//! in `~/.config/omarchy-meeting-recorder/config.toml`, and is
//! `large-v3-turbo` otherwise. A name from `MODELS` is looked for in the app's
//! own model folder and in voxtype's (same files, no need to have them twice),
//! and downloaded when it is in neither. A path to a `.bin` file is used as is.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use gtk::glib;
use whisper_rs::DtwModelPreset;

use crate::transcribe::{Abort, Events, download, models_dir};

pub const DEFAULT: &str = "large-v3-turbo";

/// A fixed revision, so a later change upstream never reaches the app unseen.
const REPO: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1";

pub struct Model {
    pub name: &'static str,
    /// Download size in MB, for the text in the app.
    pub size_mb: u32,
    /// Of the file at `REPO`'s revision; a download that differs is thrown away.
    sha256: &'static str,
    preset: DtwModelPreset,
}

pub const MODELS: [Model; 10] = [
    Model {
        name: "tiny",
        size_mb: 75,
        sha256: "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
        preset: DtwModelPreset::Tiny,
    },
    Model {
        name: "tiny.en",
        size_mb: 75,
        sha256: "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f",
        preset: DtwModelPreset::TinyEn,
    },
    Model {
        name: "base",
        size_mb: 142,
        sha256: "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
        preset: DtwModelPreset::Base,
    },
    Model {
        name: "base.en",
        size_mb: 142,
        sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
        preset: DtwModelPreset::BaseEn,
    },
    Model {
        name: "small",
        size_mb: 466,
        sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
        preset: DtwModelPreset::Small,
    },
    Model {
        name: "small.en",
        size_mb: 466,
        sha256: "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
        preset: DtwModelPreset::SmallEn,
    },
    Model {
        name: "medium",
        size_mb: 1500,
        sha256: "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
        preset: DtwModelPreset::Medium,
    },
    Model {
        name: "medium.en",
        size_mb: 1500,
        sha256: "cc37e93478338ec7700281a7ac30a10128929eb8f427dda2e865faa8f6da4356",
        preset: DtwModelPreset::MediumEn,
    },
    Model {
        name: "large-v3",
        size_mb: 3100,
        sha256: "64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2",
        preset: DtwModelPreset::LargeV3,
    },
    Model {
        name: "large-v3-turbo",
        size_mb: 1600,
        sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
        preset: DtwModelPreset::LargeV3Turbo,
    },
];

/// Set by `--model`; wins over the config file.
static OVERRIDE: Mutex<Option<String>> = Mutex::new(None);
/// Held while a model downloads, so a second caller waits instead of
/// fetching the same file again.
static DOWNLOADING: Mutex<()> = Mutex::new(());

pub fn set_override(name: &str) {
    *OVERRIDE.lock().unwrap() = Some(name.trim().to_owned());
}

pub fn config_file() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| glib::home_dir().join(".config"))
        .join(crate::APP_NAME)
        .join("config.toml")
}

/// The configured model: a name from `MODELS` or a path to a model file.
pub fn configured() -> String {
    if let Some(name) = OVERRIDE.lock().unwrap().clone() {
        return name;
    }
    config_value("model").unwrap_or_else(|| DEFAULT.to_owned())
}

/// A top-level `key = "value"` setting of the config file, without its quotes
/// or a trailing comment; None when it is not there or empty. The `[[action]]`
/// tables below the settings are the business of `actions`.
pub fn config_value(key: &str) -> Option<String> {
    std::fs::read_to_string(config_file())
        .ok()
        .and_then(|text| config_value_in(&text, key))
}

pub fn config_value_in(text: &str, key: &str) -> Option<String> {
    text.lines()
        .take_while(|line| !line.trim_start().starts_with('['))
        .find_map(|line| {
            let (k, value) = line.split_once('=')?;
            (k.trim() == key).then(|| {
                let value = value.split('#').next().unwrap_or("");
                value.trim().trim_matches('"').trim_matches('\'').to_owned()
            })
        })
        .filter(|value| !value.is_empty())
}

fn known(name: &str) -> Option<&'static Model> {
    let name = name.strip_prefix("ggml-").unwrap_or(name);
    let name = name.strip_suffix(".bin").unwrap_or(name);
    MODELS.iter().find(|m| m.name == name)
}

fn file_name(model: &Model) -> String {
    format!("ggml-{}.bin", model.name)
}

fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| glib::home_dir().join(".local/share"))
}

/// A complete model file: at least most of its expected size.
fn usable(path: &Path, model: Option<&Model>) -> bool {
    let min = model.map_or(10_000_000, |m| u64::from(m.size_mb) * 800_000);
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() >= min)
}

/// The configured model's file, when it is on disk.
pub fn find() -> Option<PathBuf> {
    let name = configured();
    match known(&name) {
        Some(model) => [models_dir(), data_dir().join("voxtype/models")]
            .into_iter()
            .map(|dir| dir.join(file_name(model)))
            .find(|path| usable(path, Some(model))),
        None => {
            let path = PathBuf::from(&name);
            usable(&path, None).then_some(path)
        }
    }
}

/// For the app: the model's name and its size when it still has to be downloaded.
pub fn missing() -> Option<(String, u32)> {
    if find().is_some() {
        return None;
    }
    let name = configured();
    known(&name).map(|m| (m.name.to_owned(), m.size_mb))
}

/// The model file, downloaded first when needed. Blocking.
pub fn ensure(events: &Events, abort: &Abort) -> Result<PathBuf, String> {
    let _one_at_a_time = DOWNLOADING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(path) = find() {
        return Ok(path);
    }
    let name = configured();
    let Some(model) = known(&name) else {
        let names: Vec<&str> = MODELS.iter().map(|m| m.name).collect();
        return Err(format!(
            "unknown model \"{name}\": use one of {} or a path to a model file",
            names.join(", ")
        ));
    };
    let target = models_dir().join(file_name(model));
    download(
        &format!("{REPO}/{}", file_name(model)),
        &target,
        "Downloading model",
        model.sha256,
        events,
        abort,
    )?;
    Ok(target)
}

/// The attention-head preset for word times; None for a model file of unknown kind.
pub fn dtw_preset() -> Option<DtwModelPreset> {
    known(&configured()).map(|m| m.preset.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_file_names_both_resolve() {
        assert_eq!(known("large-v3").map(|m| m.name), Some("large-v3"));
        assert_eq!(known("ggml-small.en.bin").map(|m| m.name), Some("small.en"));
        assert!(known("gpt-5").is_none());
    }

    #[test]
    fn every_model_has_a_whole_checksum() {
        let hex = |b: u8| matches!(b, b'0'..=b'9' | b'a'..=b'f');
        for model in &MODELS {
            assert_eq!(model.sha256.len(), 64, "{}", model.name);
            assert!(model.sha256.bytes().all(hex), "{}", model.name);
        }
    }
}
