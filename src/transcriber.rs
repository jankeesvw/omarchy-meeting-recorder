//! A transcriber of your own, instead of whisper.cpp in the app.
//!
//! `transcriber = "…"` in config.toml names a command. When it is set, the app
//! loads no speech model and hands that command the audio when a meeting
//! stops: for a recording the two levelled tracks from `.tracks`, for an
//! imported file the one file. The command prints the transcript on stdout in
//! the app's own line format, `**[01:23] You:** What you said.`, with the
//! labels the app itself uses (You and Remote, or You 1, Remote 2 and so on,
//! for a recording; Speaker 1, Speaker 2 for an import). That is what
//! `omarchy-meeting-recorder transcribe` prints, and that command always uses
//! whisper, so it can be the transcriber without a loop. Lines on stderr show
//! as the stage while it runs; one that ends in a percentage moves the bar.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::transcribe::{Abort, CANCELLED, Event, Events, LANGUAGES, Segment, Transcript};

/// The command from config.toml, if any.
pub fn configured() -> Option<String> {
    crate::models::config_value("transcriber")
}

/// What the transcriber is given.
pub enum Input<'a> {
    /// A recording: the mic and the computer audio, two files.
    Tracks { mic: &'a Path, computer: &'a Path },
    /// An imported file, with the number of speakers asked for.
    Single {
        audio: &'a Path,
        speakers: Option<usize>,
    },
}

/// Runs `command` on `input` and reads the transcript it prints. Blocking;
/// `abort` kills the command. `duration_secs` is the meeting's length, which
/// the command cannot know for sure and the transcript heading needs.
pub fn transcribe(
    command: &str,
    input: Input,
    language: &str,
    duration_secs: i64,
    events: &Events,
    abort: &Abort,
) -> Result<Transcript, String> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).arg("meeting-transcriber");
    match &input {
        Input::Tracks { mic, computer } => {
            cmd.arg(mic).arg(computer);
            cmd.env("MEETING_MIC", mic)
                .env("MEETING_COMPUTER", computer);
        }
        Input::Single { audio, speakers } => {
            cmd.arg(audio);
            cmd.env("MEETING_AUDIO", audio);
            if let Some(n) = speakers {
                cmd.env("MEETING_SPEAKER_COUNT", n.to_string());
            }
        }
    }
    cmd.env("MEETING_LANGUAGE", language)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let _ = events.send_blocking(Event::Stage("Running your transcriber".into()));
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not run your transcriber: {e}"))?;

    // What it says goes to the animation as it comes; what it prints is the
    // transcript, read to the end. Each on its own thread, so neither pipe
    // fills up while the other is waited on.
    let stderr = child.stderr.take();
    let told = events.clone();
    let stderr_reader = std::thread::spawn(move || {
        let mut last = String::new();
        let Some(stderr) = stderr else { return last };
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(progress) = percentage(line) {
                let _ = told.send_blocking(Event::Progress(progress));
            }
            let _ = told.send_blocking(Event::Stage(line.chars().take(120).collect()));
            last = line.to_owned();
        }
        last
    });
    let stdout = child.stdout.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut stdout) = stdout {
            let _ = stdout.read_to_string(&mut text);
        }
        text
    });

    let status = loop {
        if abort.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CANCELLED.to_owned());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let said = stderr_reader.join().unwrap_or_default();
    let printed = stdout_reader.join().unwrap_or_default();
    if !status.success() {
        return Err(if said.is_empty() {
            format!("your transcriber exited with {status}")
        } else {
            format!("your transcriber failed: {said}")
        });
    }

    let segments = from_markdown(&printed);
    for segment in &segments {
        let _ = events.send_blocking(Event::Segment(format!(
            "{}: {}",
            segment.speaker, segment.text
        )));
    }
    let _ = events.send_blocking(Event::Progress(1.0));
    let language = language_in(&printed).unwrap_or_else(|| {
        if language == "auto" {
            "unknown".to_owned()
        } else {
            language.to_owned()
        }
    });
    Ok(Transcript {
        segments,
        language,
        duration_secs,
    })
}

/// The transcript lines in what the command printed, in order. Anything that
/// is not a `**[time] Speaker:** text` line (headings, chapters, chatter) is
/// skipped. A line ends where the next one starts.
pub fn from_markdown(text: &str) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("**[") else {
            continue;
        };
        let Some((time, rest)) = rest.split_once("] ") else {
            continue;
        };
        let Some((speaker, text)) = rest.split_once(":** ") else {
            continue;
        };
        let (speaker, text) = (speaker.trim(), text.trim());
        if speaker.is_empty() || text.is_empty() {
            continue;
        }
        let Some(start_ms) = clock_to_ms(time) else {
            continue;
        };
        if let Some(previous) = segments.last_mut() {
            previous.end_ms = previous.end_ms.max(start_ms);
        }
        segments.push(Segment {
            start_ms,
            end_ms: start_ms,
            speaker: speaker.to_owned(),
            text: text.to_owned(),
        });
    }
    segments
}

/// `01:23` or `1:02:03` to milliseconds; None for anything else.
fn clock_to_ms(clock: &str) -> Option<i64> {
    let parts: Vec<i64> = clock
        .split(':')
        .map(|part| part.trim().parse::<i64>().ok())
        .collect::<Option<_>>()?;
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    Some(parts.iter().fold(0, |total, part| total * 60 + part) * 1000)
}

/// The language the command named in a `- **Language:** English` line, as
/// the app writes it, as a whisper code; None when there is no such line or
/// the name is not one the dropdown knows.
fn language_in(text: &str) -> Option<String> {
    let name = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("- **Language:** "))?
        .trim();
    LANGUAGES
        .iter()
        .find(|(code, label)| {
            *code != "auto" && (label.eq_ignore_ascii_case(name) || *code == name)
        })
        .map(|(code, _)| (*code).to_owned())
}

/// A percentage at the end of a progress line, as 0.0 to 1.0.
fn percentage(line: &str) -> Option<f64> {
    let number = line
        .rsplit(|c: char| c.is_whitespace())
        .next()?
        .strip_suffix('%')?;
    let value: f64 = number.trim().parse().ok()?;
    (0.0..=100.0).contains(&value).then_some(value / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transcriber_is_a_top_level_setting() {
        let config = r#"
model = "small"
transcriber = "~/bin/transcribe-on-my-mac"   # a comment

[[action]]
name = "Store"
command = "~/bin/store"
transcriber = "not this one"
"#;
        assert_eq!(
            crate::models::config_value_in(config, "transcriber").as_deref(),
            Some("~/bin/transcribe-on-my-mac")
        );
        assert_eq!(
            crate::models::config_value_in(config, "model").as_deref(),
            Some("small")
        );
        assert_eq!(
            crate::models::config_value_in("model = \"\"", "model"),
            None
        );
        assert_eq!(crate::models::config_value_in("", "transcriber"), None);
    }

    #[test]
    fn transcript_lines_become_segments_and_the_rest_is_skipped() {
        let printed = "# Transcript\n\n- **Date:** 2026-10-09 09:00\n- **Language:** Dutch\n\n\
            ## Chapters\n\n- [00:00] Opening\n\n## Transcript\n\n\
            **[00:01] You:** Hi.\n\n**[00:03] Remote 2:** Hallo daar.\n\nchatter\n\n\
            **[1:02:03] You 1:** Late.\n";
        let segments = from_markdown(printed);
        assert_eq!(segments.len(), 3);
        assert_eq!(
            (
                segments[0].start_ms,
                segments[0].end_ms,
                segments[0].speaker.as_str()
            ),
            (1000, 3000, "You")
        );
        assert_eq!(segments[1].text, "Hallo daar.");
        assert_eq!(segments[2].start_ms, 3_723_000);
        assert_eq!(language_in(printed).as_deref(), Some("nl"));
        assert_eq!(language_in("- **Language:** Klingon"), None);
        assert_eq!(percentage("transcribing 40%"), Some(0.4));
        assert_eq!(percentage("40 percent done"), None);
    }

    #[test]
    fn a_shell_command_is_a_transcriber() {
        let (tx, rx) = async_channel::unbounded();
        let mic = Path::new("/tmp/mic.ogg");
        let computer = Path::new("/tmp/computer.ogg");
        let command = r#"echo "half way 50%" >&2; echo "- **Language:** English"; echo "**[00:01] You:** Got $1 and $2 in $MEETING_LANGUAGE""#;
        let transcript = transcribe(
            command,
            Input::Tracks { mic, computer },
            "auto",
            90,
            &tx,
            &Abort::default(),
        )
        .unwrap();
        assert_eq!(transcript.language, "en");
        assert_eq!(transcript.duration_secs, 90);
        assert_eq!(transcript.segments.len(), 1);
        assert_eq!(
            transcript.segments[0].text,
            "Got /tmp/mic.ogg and /tmp/computer.ogg in auto"
        );
        let mut progress = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let Event::Progress(p) = event {
                progress.push(p);
            }
        }
        assert_eq!(progress, [0.5, 1.0]);

        let (tx, _rx) = async_channel::unbounded();
        let failed = transcribe(
            "echo nothing here; echo no model >&2; exit 2",
            Input::Single {
                audio: Path::new("/tmp/call.mp3"),
                speakers: Some(3),
            },
            "en",
            10,
            &tx,
            &Abort::default(),
        );
        assert_eq!(
            failed.err().as_deref(),
            Some("your transcriber failed: no model")
        );
    }
}
