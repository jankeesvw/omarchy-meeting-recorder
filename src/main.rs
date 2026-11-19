//! Meeting Recorder: records a meeting in two tracks (mic and computer
//! audio), transcribes it with whisper.cpp after the call, and streams live
//! levels to a bar widget.

mod actions;
mod agent;
mod animation;
mod audio;
mod bar_widget;
mod chapters;
mod diarize;
mod export;
mod find;
mod ipc;
mod meeting;
mod models;
mod nemotron;
mod player;
mod trim;
mod settings;
mod notes;
mod theme;
mod transcribe;
mod ui;
mod webcam;

use gtk::glib;

pub const APP_ID: &str = "com.jankeesvw.OmarchyMeetingRecorder";
pub const APP_NAME: &str = "omarchy-meeting-recorder";

/// The flags that open the recorder or start it: `--name <name>` and
/// `--notes <file>`, each also as `--flag=value`. Words outside a flag join
/// the name, as older scripts pass it (`start Weekly sync`).
#[derive(Debug, Default, PartialEq)]
struct Flags {
    name: Vec<String>,
    notes: Option<String>,
}

fn flags(args: &[String]) -> Result<Flags, String> {
    let mut flags = Flags::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let (flag, value) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        match flag {
            "--name" | "--notes" => {
                let value = value
                    .or_else(|| args.next().cloned())
                    .ok_or_else(|| format!("{flag} needs a value"))?;
                if flag == "--name" {
                    flags.name.push(value);
                } else {
                    flags.notes = Some(value);
                }
            }
            _ if flag.starts_with("--") => return Err(format!("unknown flag '{flag}'")),
            _ => flags.name.push(arg.clone()),
        }
    }
    Ok(flags)
}

/// Opens the recorder with the notes and the name in `args`, and starts
/// recording when `start` is set.
fn open_with(args: &[String], start: bool) -> glib::ExitCode {
    let flags = match flags(args) {
        Ok(flags) => flags,
        Err(err) => {
            eprintln!("{APP_NAME}: {err}, see --help");
            return glib::ExitCode::from(2);
        }
    };
    let mut lines = Vec::new();
    if let Some(file) = flags.notes {
        match std::fs::canonicalize(file).ok().and_then(|path| ipc::notes_line(&path)) {
            Some(line) => lines.push(line),
            None => {
                eprintln!("{APP_NAME}: --notes needs a text file that exists");
                return glib::ExitCode::from(2);
            }
        }
    }
    if start {
        lines.push(ipc::start_line(&flags.name));
    } else {
        lines.extend(ipc::name_line(&flags.name));
    }
    if lines.is_empty() {
        return ui::run(None);
    }
    send_or_open(lines)
}

/// Sends `lines` to the running app, or opens the app and sends them as soon as it listens.
fn send_or_open(lines: Vec<String>) -> glib::ExitCode {
    let Some(first) = lines.first().cloned() else {
        return glib::ExitCode::SUCCESS;
    };
    if ipc::send(&first) {
        lines[1..].iter().for_each(|line| {
            ipc::send(line);
        });
        return glib::ExitCode::SUCCESS;
    }
    std::thread::spawn(move || {
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if ipc::send(&first) {
                lines[1..].iter().for_each(|line| {
                    ipc::send(line);
                });
                return;
            }
        }
    });
    ui::run(None)
}

fn main() -> glib::ExitCode {
    match std::env::args().nth(1).as_deref() {
        None => ui::run(None),
        Some("--version" | "-V") => {
            let backend = if cfg!(feature = "vulkan") {
                "Vulkan support; CPU fallback"
            } else {
                "CPU-only"
            };
            println!("{APP_NAME} {} ({backend})", env!("CARGO_PKG_VERSION"));
            glib::ExitCode::SUCCESS
        }
        Some("watch") => {
            ipc::watch();
            glib::ExitCode::SUCCESS
        }
        Some("start") => open_with(&std::env::args().skip(2).collect::<Vec<_>>(), true),
        Some(command @ ("stop" | "compact" | "pause")) => {
            if ipc::send(command) {
                glib::ExitCode::SUCCESS
            } else {
                eprintln!("{APP_NAME}: the recorder is not running");
                glib::ExitCode::FAILURE
            }
        }
        Some("wait") => {
            if ipc::wait(std::env::args().any(|arg| arg == "--audio")) {
                glib::ExitCode::SUCCESS
            } else {
                eprintln!("{APP_NAME}: the recorder closed before the meeting was done");
                glib::ExitCode::FAILURE
            }
        }
        Some("transcribe-file") => {
            transcribe::cli_file(&std::env::args().skip(2).collect::<Vec<_>>())
        }
        Some("diarize") => diarize::cli(&std::env::args().skip(2).collect::<Vec<_>>()),
        Some("transcribe") => transcribe::cli(&std::env::args().skip(2).collect::<Vec<_>>()),
        Some("action") => actions::cli(&std::env::args().skip(2).collect::<Vec<_>>()),
        // A new window in the running app, or the app itself when it is not running.
        Some("new-window") => {
            if ipc::send("new-window") {
                glib::ExitCode::SUCCESS
            } else {
                ui::run(None)
            }
        }
        Some("ask") => agent::cli(&std::env::args().skip(2).collect::<Vec<_>>()),
        Some("-h" | "--help") => {
            println!(
                "Usage: {APP_NAME} [[start] [--name <name>] [--notes <file>] | stop | pause | compact | watch | wait [--audio] | transcribe <mic> <computer> [--language xx]]"
            );
            println!();
            println!("  (no command)  open the recorder, ready to record");
            println!("  --name <name>   name the meeting about to be recorded, whoever presses Start");
            println!(
                "  --notes <file>  show the file as meeting notes next to the recorder, to read from and edit while recording"
            );
            println!("  <meeting>     open a .meeting-recorder file or a meeting folder");
            println!("  start         start recording, opening the recorder if needed; takes --name and --notes too");
            println!("  stop          stop the running recording (for a keybinding)");
            println!("  compact       switch the recording window between full and compact");
            println!("  pause         pause or resume the running recording");
            println!("  watch         stream the recorder state as NDJSON, for the bar widget");
            println!(
                "  wait          wait until the recording is done and print its folder (--audio: once the audio is saved)"
            );
            println!("  transcribe    transcribe two tracks and print the transcript as Markdown");
            println!(
                "  ask           run a prompt over stdin through the default agent, without tools"
            );
            println!("  action        run one of your actions on a meeting folder");
            println!(
                "  new-window    open another window, for a second meeting (Ctrl+N in the app)"
            );
            glib::ExitCode::SUCCESS
        }
        Some(flag) if flag.starts_with("--") => {
            open_with(&std::env::args().skip(1).collect::<Vec<_>>(), false)
        }
        Some(path)
            if path.ends_with(&format!(".{}", meeting::EXTENSION))
                || std::path::Path::new(path).is_dir() =>
        {
            ui::run(Some(path))
        }
        Some(other) => {
            eprintln!("{APP_NAME}: unknown command '{other}', see --help");
            glib::ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{flags, Flags};

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn flags_take_a_value_after_them_or_after_an_equals_sign() {
        assert_eq!(
            flags(&args(&["--notes", "script.md", "--name=Voice-over prioriteit"])),
            Ok(Flags { name: args(&["Voice-over prioriteit"]), notes: Some("script.md".into()) })
        );
        assert_eq!(
            flags(&args(&["--name", "Voice-over", "--notes=script.md"])),
            Ok(Flags { name: args(&["Voice-over"]), notes: Some("script.md".into()) })
        );
    }

    #[test]
    fn words_outside_a_flag_are_the_name_as_before() {
        assert_eq!(flags(&args(&["Weekly", "sync"])).unwrap().name, args(&["Weekly", "sync"]));
        assert_eq!(flags(&[]), Ok(Flags::default()));
    }

    #[test]
    fn a_flag_without_a_value_or_unknown_is_an_error() {
        assert!(flags(&args(&["--notes"])).is_err());
        assert!(flags(&args(&["--note=script.md"])).is_err());
    }
}
