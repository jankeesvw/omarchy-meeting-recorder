//! A command of your own that runs when a transcript is done.
//!
//! Set `after_transcript` in `config.toml` and it runs with the meeting folder
//! as its argument, once the transcript (and its chapters, when an agent makes
//! them) is written. That is the place to file the meeting in a notes app,
//! summarize it, or copy it somewhere. The recorder does not wait for it: the
//! command runs in its own process group, so it also finishes when the app
//! quits.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::APP_NAME;

/// The configured command, None when there is none.
pub fn configured() -> Option<String> {
    crate::models::config_value("after_transcript")
}

/// Runs the configured command for the meeting in `dir`, if there is one.
pub fn run(dir: &Path) {
    let Some(command) = configured() else {
        return;
    };
    let transcript = dir.join("transcript.md");
    let mut child = Command::new("sh");
    child
        // The folder is "$1", so the command can use it however it likes;
        // appended here, it also works as a plain program name.
        .arg("-c")
        .arg(format!("{command} \"$1\""))
        .arg("sh")
        .arg(dir)
        .env("MEETING_DIR", dir)
        .env("MEETING_TRANSCRIPT", &transcript)
        .stdin(Stdio::null());
    if let Some(manifest) = crate::meeting::find(dir) {
        child.env("MEETING_MANIFEST", manifest);
    }
    std::os::unix::process::CommandExt::process_group(&mut child, 0);
    match child.spawn() {
        Ok(mut child) => {
            // Reaped on a thread of its own, so a slow command never holds up the app.
            std::thread::spawn(move || match child.wait() {
                Ok(status) if !status.success() => {
                    eprintln!("{APP_NAME}: after_transcript exited with {status}");
                }
                Err(e) => eprintln!("{APP_NAME}: after_transcript: {e}"),
                _ => {}
            });
        }
        Err(e) => eprintln!("{APP_NAME}: could not run after_transcript: {e}"),
    }
}
