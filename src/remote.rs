//! Transcription on another machine, picked under "Transcribe on" (an SSH
//! host). The host runs the same app (its own model and config); this copies the
//! meeting's Opus tracks there over SSH, runs `transcribe --json` and reads
//! the transcript back. Progress and live lines stream back as they happen.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;

use crate::transcribe::{Abort, CANCELLED, Event, Events, Transcript};

/// Fail fast instead of hanging when the host is off or asks for a password.
const SSH_OPTS: [&str; 4] = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"];

pub fn host() -> Option<String> {
    Some(crate::settings::load_remote()).filter(|host| !host.is_empty())
}

/// The named hosts of `~/.ssh/config`, to choose from.
pub fn ssh_hosts() -> Vec<String> {
    let config = std::fs::read_to_string(gtk::glib::home_dir().join(".ssh/config"));
    hosts_in(&config.unwrap_or_default())
}

// ponytail: `Include`d files and `Match` blocks are not followed; add when a host lives there.
fn hosts_in(config: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    for line in config.lines() {
        let mut words = line.split_whitespace();
        if !words
            .next()
            .is_some_and(|key| key.eq_ignore_ascii_case("host"))
        {
            continue;
        }
        // Patterns like `*` or `!bastion` match hosts, they don't name one.
        // One machine per line: `Host MiniPC server` lists MiniPC only.
        let first = words.find(|w| !w.contains(['*', '?', '!']));
        if let Some(host) = first.filter(|host| !hosts.iter().any(|h| h == host)) {
            hosts.push(host.to_owned());
        }
    }
    hosts
}

pub fn transcribe(
    host: &str,
    mic: &Path,
    computer: &Path,
    language: &str,
    events: &Events,
    abort: &Abort,
) -> Result<Transcript, String> {
    let _ = events.send_blocking(Event::Stage(format!("Sending to {host}")));
    let dir = ssh_output(host, "mktemp -d")?;
    let result =
        send(host, &dir, mic, computer).and_then(|()| run(host, &dir, language, events, abort));
    let _ = Command::new("ssh")
        .args(SSH_OPTS)
        .arg(host)
        .arg(format!("rm -r {dir}"))
        .status();
    result
}

fn ssh_output(host: &str, command: &str) -> Result<String, String> {
    let out = Command::new("ssh")
        .args(SSH_OPTS)
        .arg(host)
        .arg(command)
        .output()
        .map_err(|e| format!("could not run ssh: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ssh {host} {command}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn send(host: &str, dir: &str, mic: &Path, computer: &Path) -> Result<(), String> {
    let status = Command::new("scp")
        .arg("-q")
        .args(SSH_OPTS)
        .arg(mic)
        .arg(computer)
        .arg(format!("{host}:{dir}/"))
        .status()
        .map_err(|e| format!("could not run scp: {e}"))?;
    if !status.success() {
        return Err(format!("could not copy the tracks to {host}"));
    }
    Ok(())
}

fn run(
    host: &str,
    dir: &str,
    language: &str,
    events: &Events,
    abort: &Abort,
) -> Result<Transcript, String> {
    let command = format!(
        "{} transcribe --json --language '{language}' {dir}/mic.ogg {dir}/computer.ogg",
        crate::APP_NAME
    );
    let mut child = Command::new("ssh")
        .args(SSH_OPTS)
        .arg(host)
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run ssh: {e}"))?;
    // stdout on its own thread: a long transcript fills the pipe before the
    // remote side closes stderr.
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        stdout.read_to_string(&mut text).map(|_| text)
    });
    let mut last_error = String::new();
    for line in BufReader::new(child.stderr.take().expect("stderr is piped")).lines() {
        if abort.load(Ordering::Relaxed) {
            // The remote process dies on its next write to the closed pipe.
            let _ = child.kill();
            let _ = child.wait();
            return Err(CANCELLED.into());
        }
        let Ok(line) = line else { break };
        match forward(&line) {
            Some(event) => {
                let _ = events.send_blocking(event);
            }
            None => last_error = line,
        }
    }
    let status = child.wait().map_err(|e| format!("ssh {host}: {e}"))?;
    let stdout = reader.join().ok().and_then(Result::ok).unwrap_or_default();
    if !status.success() {
        return Err(format!("transcription on {host} failed: {last_error}"));
    }
    crate::transcribe::transcript_from_json(&stdout)
}

/// One JSON event line of `transcribe --json`; None for anything else.
fn forward(line: &str) -> Option<Event> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if let Some(stage) = value["stage"].as_str() {
        return Some(Event::Stage(stage.to_owned()));
    }
    if let Some(progress) = value["progress"].as_f64() {
        return Some(Event::Progress(progress));
    }
    value["segment"]
        .as_str()
        .map(|text| Event::Segment(text.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_event_lines_only() {
        assert!(
            matches!(forward(r#"{"stage":"Transcribing"}"#), Some(Event::Stage(s)) if s == "Transcribing")
        );
        assert!(matches!(forward(r#"{"progress":0.5}"#), Some(Event::Progress(p)) if p == 0.5));
        assert!(
            matches!(forward(r#"{"segment":"You: hi"}"#), Some(Event::Segment(s)) if s == "You: hi")
        );
        assert!(forward("omarchy-meeting-recorder: could not load the model").is_none());
        assert!(forward(r#"{"other":1}"#).is_none());
    }

    #[test]
    fn named_hosts_only() {
        let config = "Host *\n  ServerAliveInterval 30\nHost MiniPC server\n  HostName 192.168.2.10\n\
                      host gh-*.example !bastion work\nHost MiniPC\n";
        assert_eq!(hosts_in(config), ["MiniPC", "work"]);
        assert!(hosts_in("").is_empty());
    }
}
