//! Actions: your own scripts, run on a finished meeting from the done page.
//!
//! They live in `config.toml`, each with a name for the menu and a command:
//!
//! ```toml
//! [[action]]
//! name = "Copy to Obsidian"
//! command = "~/.local/bin/meeting-to-obsidian"
//! ```
//!
//! The command runs through `sh -c` in the meeting folder, with that folder as
//! `$1` and the meeting described in `MEETING_*` variables. What it prints last
//! is shown when it is done; a link there (web or `obsidian://`) gets an Open
//! button.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::meeting::Manifest;

#[derive(Clone, Debug, PartialEq)]
pub struct Action {
    pub name: String,
    pub command: String,
}

/// The actions in the config file, in its order. Read every time, so an edit
/// shows up without restarting the app.
pub fn load() -> Vec<Action> {
    std::fs::read_to_string(crate::models::config_file())
        .map(|text| parse(&text))
        .unwrap_or_default()
}

/// The `[[action]]` tables of a config file. A line format rather than a full
/// TOML parser: `key = "value"` pairs, `#` comments, nothing nested.
fn parse(text: &str) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut current: Option<(String, String)> = None;
    let mut finish = |current: &mut Option<(String, String)>| {
        if let Some((name, command)) = current.take()
            && !name.is_empty()
            && !command.is_empty()
        {
            actions.push(Action { name, command });
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            finish(&mut current);
            if line == "[[action]]" {
                current = Some((String::new(), String::new()));
            }
            continue;
        }
        let Some((name, command)) = current.as_mut() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim() {
            "name" => *name = value,
            "command" => *command = value,
            _ => {}
        }
    }
    finish(&mut current);
    actions
}

/// A TOML string: `"..."` with backslash escapes, or `'...'` taken literally.
fn unquote(value: &str) -> String {
    if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.split('\'').next()) {
        return inner.to_owned();
    }
    let Some(rest) = value.strip_prefix('"') else {
        return value.split('#').next().unwrap_or("").trim().to_owned();
    };
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => break,
            },
            c => out.push(c),
        }
    }
    out
}

/// How an action ended: the last thing it printed, and a link in it if any.
pub struct Outcome {
    pub message: String,
    pub url: Option<String>,
}

/// Runs `action` on the meeting in `dir`. Blocking; the caller runs it off the
/// main thread.
pub fn run(action: &Action, dir: &Path, manifest: &Manifest) -> Result<Outcome, String> {
    let date = gtk::glib::DateTime::from_unix_local(manifest.started_at)
        .and_then(|t| t.format("%Y-%m-%d %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default();
    let audio = ["audio.ogg", "mic.ogg", "computer.ogg"]
        .iter()
        .map(|f| dir.join(f))
        .find(|p| p.exists());
    let output = Command::new("sh")
        .arg("-c")
        .arg(&action.command)
        .arg("meeting-action")
        .arg(dir)
        .current_dir(dir)
        .env("MEETING_DIR", dir)
        .env("MEETING_TRANSCRIPT", dir.join("transcript.md"))
        .env(
            "MEETING_MANIFEST",
            crate::meeting::find(dir).unwrap_or_default(),
        )
        .env("MEETING_TITLE", &manifest.title)
        .env("MEETING_DATE", &date)
        .env("MEETING_STARTED_AT", manifest.started_at.to_string())
        .env("MEETING_DURATION", manifest.duration_secs.to_string())
        .env("MEETING_LANGUAGE", &manifest.language)
        .env("MEETING_SPEAKERS", manifest.speakers.join("\n"))
        .env("MEETING_AUDIO", audio.unwrap_or_default())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let why = last_line(&stderr)
            .or_else(|| last_line(&stdout))
            .unwrap_or_else(|| format!("exited with {}", output.status));
        return Err(why);
    }
    let message = last_line(&stdout).unwrap_or_else(|| "Done".to_owned());
    Ok(Outcome {
        url: find_url(&message),
        message,
    })
}

fn last_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(200).collect())
}

/// A link to open: a web page, or a note in Obsidian.
fn find_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|w| {
            ["https://", "http://", "obsidian://"]
                .iter()
                .any(|s| w.starts_with(s))
        })
        .map(|w| w.trim_end_matches(['.', ',', ')', '"', '\'']).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_actions_and_skips_the_rest() {
        let config = r#"
model = "large-v3-turbo"  # not an action

[[action]]
name = "Copy to Obsidian"
command = "~/.local/bin/meeting-to-obsidian"

[[action]]
name = 'Publish'   # a comment
command = 'publish "$1" --secret'

[[action]]
name = "No command, ignored"

[other]
name = "not an action"
"#;
        assert_eq!(
            parse(config),
            [
                Action {
                    name: "Copy to Obsidian".into(),
                    command: "~/.local/bin/meeting-to-obsidian".into(),
                },
                Action {
                    name: "Publish".into(),
                    command: "publish \"$1\" --secret".into(),
                },
            ]
        );
    }

    #[test]
    fn a_link_in_the_output_is_found() {
        assert_eq!(
            find_url("Published: https://gist.github.com/abc123."),
            Some("https://gist.github.com/abc123".into())
        );
        assert_eq!(
            find_url("Saved obsidian://open?vault=Writing&file=Meetings%2FWeekly"),
            Some("obsidian://open?vault=Writing&file=Meetings%2FWeekly".into())
        );
        assert_eq!(find_url("Copied to the vault"), None);
    }
}
