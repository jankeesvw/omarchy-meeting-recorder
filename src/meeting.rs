//! The `.meeting-recorder` file: a small JSON manifest inside every meeting folder.
//!
//! The folder stays a plain folder, so the audio and the transcript are easy to
//! reach with any other tool. The manifest carries what the folder cannot say
//! for itself (the title, when it started, the chosen format and language) and
//! has its own MIME type, so double-clicking it opens the meeting in the app.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::chapters::Chapter;
use crate::export::Format;

pub const EXTENSION: &str = "meeting-recorder";
pub const DEFAULT_YOU: &str = "You";
pub const DEFAULT_REMOTE: &str = "Remote";

#[derive(Clone, Debug)]
pub struct Manifest {
    pub title: String,
    pub started_at: i64,
    pub duration_secs: i64,
    pub format: Format,
    /// The language picked for the transcript: a code from `LANGUAGES`, or "auto".
    pub language: String,
    /// What the speakers are called, in the order of their default labels:
    /// for a recording [microphone, computer audio], for an imported file
    /// [Speaker 1, Speaker 2, ...].
    pub speakers: Vec<String>,
    /// The original file name when the meeting was imported instead of recorded.
    pub imported: Option<String>,
    /// How many speakers were asked for on import; None means automatic.
    pub speaker_count: Option<usize>,
    /// The whisper model the transcript was made with.
    pub model: Option<String>,
    /// Chapters made by an agent, empty when there are none.
    pub chapters: Vec<Chapter>,
    /// Which agent made them, e.g. "claude".
    pub chapters_by: Option<String>,
}

impl Manifest {
    fn to_json(&self) -> Value {
        json!({
            "app": crate::APP_NAME,
            "version": 1,
            "title": self.title,
            "started_at": self.started_at,
            "duration_secs": self.duration_secs,
            "format": self.format.key(),
            "language": self.language,
            "speakers": self.speakers,
            "imported": self.imported,
            "speaker_count": self.speaker_count,
            "model": self.model,
            "chapters": self.chapters.iter()
                .map(|c| json!({ "start_ms": c.start_ms, "title": c.title }))
                .collect::<Vec<_>>(),
            "chapters_by": self.chapters_by,
        })
    }

    fn from_json(value: &Value) -> Option<Manifest> {
        Some(Manifest {
            title: value["title"].as_str()?.to_owned(),
            started_at: value["started_at"].as_i64()?,
            duration_secs: value["duration_secs"].as_i64().unwrap_or(0),
            format: Format::from_key(value["format"].as_str().unwrap_or("mono")),
            language: value["language"].as_str().unwrap_or("auto").to_owned(),
            speakers: match &value["speakers"] {
                // Before imports existed, a recording kept {"you", "remote"}.
                Value::Object(_) => vec![
                    name(&value["speakers"]["you"], DEFAULT_YOU),
                    name(&value["speakers"]["remote"], DEFAULT_REMOTE),
                ],
                Value::Array(list) => list
                    .iter()
                    .enumerate()
                    .map(|(i, v)| name(v, &format!("Speaker {}", i + 1)))
                    .collect(),
                _ => vec![DEFAULT_YOU.to_owned(), DEFAULT_REMOTE.to_owned()],
            },
            imported: value["imported"].as_str().map(str::to_owned),
            speaker_count: value["speaker_count"].as_u64().map(|n| n as usize),
            model: value["model"].as_str().map(str::to_owned),
            chapters: value["chapters"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(|c| {
                            Some(Chapter {
                                start_ms: c["start_ms"].as_i64()?,
                                title: c["title"].as_str()?.to_owned(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            chapters_by: value["chapters_by"].as_str().map(str::to_owned),
        })
    }
}

impl Manifest {
    /// The labels the transcription gives the speakers, in the order of
    /// `speakers`: You and Remote (or Remote 1, Remote 2, ... when the computer
    /// audio holds several voices) for a recording, Speaker N for an import.
    pub fn default_labels(&self) -> Vec<String> {
        if self.imported.is_some() {
            (1..=self.speakers.len().max(1))
                .map(|i| format!("Speaker {i}"))
                .collect()
        } else if self.speakers.len() > 2 {
            // Several voices on the computer audio: Remote 1, Remote 2, ...
            std::iter::once(DEFAULT_YOU.to_owned())
                .chain((1..self.speakers.len()).map(|i| format!("{DEFAULT_REMOTE} {i}")))
                .collect()
        } else {
            vec![DEFAULT_YOU.to_owned(), DEFAULT_REMOTE.to_owned()]
        }
    }
}

/// Renames several speakers at once, safe when names swap places: every old
/// name goes through a placeholder first.
pub fn relabel_all(markdown: &str, renames: &[(String, String)]) -> String {
    let mut text = markdown.to_owned();
    for (i, (from, _)) in renames.iter().enumerate() {
        text = relabel(&text, from, &format!("\u{1}{i}\u{1}"));
    }
    for (i, (_, to)) in renames.iter().enumerate() {
        text = relabel(&text, &format!("\u{1}{i}\u{1}"), to);
    }
    text
}

fn name(value: &Value, fallback: &str) -> String {
    value
        .as_str()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

/// Renames a speaker in transcript Markdown: `**[01:23] Old:**` becomes `**[01:23] New:**`.
pub fn relabel(markdown: &str, from: &str, to: &str) -> String {
    if from == to {
        return markdown.to_owned();
    }
    markdown
        .lines()
        .map(|line| {
            match line
                .strip_prefix("**[")
                .and_then(|rest| rest.split_once("] "))
            {
                Some((time, rest)) if rest.starts_with(&format!("{from}:** ")) => {
                    format!("**[{time}] {to}{}", &rest[from.len()..])
                }
                _ => line.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + if markdown.ends_with('\n') { "\n" } else { "" }
}

/// The manifest's path for `title` inside `dir`.
pub fn path_for(dir: &Path, title: &str) -> PathBuf {
    dir.join(format!("{}.{EXTENSION}", crate::ui::safe_name(title)))
}

/// Finds the manifest in a meeting folder, whatever its name.
pub fn find(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|p| p.extension().is_some_and(|e| e == EXTENSION) && p.is_file())
}

/// Writes the manifest into `dir`, replacing one with another name (after a rename).
pub fn write(dir: &Path, manifest: &Manifest) -> std::io::Result<PathBuf> {
    let target = path_for(dir, &manifest.title);
    if let Some(old) = find(dir)
        && old != target
    {
        let _ = std::fs::remove_file(old);
    }
    let text = serde_json::to_string_pretty(&manifest.to_json()).unwrap_or_default() + "\n";
    std::fs::write(&target, text)?;
    Ok(target)
}

/// Reads a meeting from a `.meeting-recorder` file or from its folder. A folder without
/// a manifest (made before manifests existed) is read from its name and files.
pub fn open(path: &Path) -> Option<(PathBuf, Manifest)> {
    let (dir, file) = if path.is_dir() {
        (path.to_path_buf(), find(path))
    } else {
        (path.parent()?.to_path_buf(), Some(path.to_path_buf()))
    };
    let from_file = file
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| Manifest::from_json(&value));
    let manifest = from_file.or_else(|| from_folder(&dir))?;
    Some((dir, manifest))
}

/// `202609241400 Weekly sync` plus whatever audio files are there.
fn from_folder(dir: &Path) -> Option<Manifest> {
    let name = dir.file_name()?.to_str()?;
    let (stamp, title) = name.split_once(' ')?;
    if stamp.len() != 12 || !stamp.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let num = |range: std::ops::Range<usize>| stamp[range].parse::<i32>().ok();
    let started = gtk::glib::DateTime::from_local(
        num(0..4)?,
        num(4..6)?,
        num(6..8)?,
        num(8..10)?,
        num(10..12)?,
        0.0,
    )
    .ok()?;
    let format = if dir.join("mic.ogg").exists() {
        Format::Separate
    } else {
        Format::Mono
    };
    Some(Manifest {
        title: title.to_owned(),
        started_at: started.to_unix(),
        duration_secs: 0,
        format,
        language: "auto".to_owned(),
        speakers: vec![DEFAULT_YOU.to_owned(), DEFAULT_REMOTE.to_owned()],
        imported: None,
        speaker_count: None,
        model: None,
        chapters: Vec::new(),
        chapters_by: None,
    })
}

#[cfg(test)]
mod tests {
    use super::relabel;

    const MD: &str = "# Weekly\n\n**[00:01] You:** Hi.\n\n**[00:03] Remote:** You: said hi.\n";

    #[test]
    fn renames_only_the_speaker_labels() {
        let out = relabel(MD, "You", "Jankees");
        assert!(out.contains("**[00:01] Jankees:** Hi."));
        assert!(out.contains("**[00:03] Remote:** You: said hi."));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn swapping_two_names_through_a_placeholder() {
        let tmp = relabel(MD, "You", "\u{1}");
        let tmp = relabel(&tmp, "Remote", "You");
        let out = relabel(&tmp, "\u{1}", "Remote");
        assert!(out.contains("**[00:01] Remote:** Hi."));
        assert!(out.contains("**[00:03] You:** You: said hi."));
    }
}
