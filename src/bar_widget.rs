//! The bar widget, offered once on the first start.
//!
//! The package installs the widget to `/usr/share`, but the Omarchy shell only
//! loads plugins from `~/.config/omarchy/plugins`, and a package has no
//! business writing in a home directory. So the app asks, and on a yes links
//! the widget there and puts it on the right of the bar.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use gtk::glib;

use crate::settings;

const ID: &str = "jankeesvw.meeting-recorder";
const SOURCE: &str = "/usr/share/omarchy-meeting-recorder/plugin";

fn target() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| glib::home_dir().join(".config"))
        .join("omarchy/plugins")
        .join(ID)
}

/// Offer on Omarchy for a package install until accepted successfully or declined.
pub fn should_offer() -> bool {
    // A failed attempt may have already created our symlink. Leave other
    // installations alone, but allow retrying the link we create in add().
    let offer = !settings::bar_widget_offered()
        && glib::find_program_in_path("omarchy").is_some()
        && PathBuf::from(SOURCE).join("manifest.json").is_file()
        && (std::fs::symlink_metadata(target()).is_err()
            || std::fs::read_link(target()).is_ok_and(|path| path == std::path::Path::new(SOURCE)));
    // Already on the bar, put there by hand or by an earlier version: nothing
    // to offer, now or later.
    if offer && enabled(run) == Some(true) {
        settings::set_bar_widget_offered();
        return false;
    }
    offer
}

/// Whether the shell has the widget on the bar; None when the shell does not answer.
fn enabled(mut run: impl FnMut(&str, &[&str]) -> Result<String, String>) -> Option<bool> {
    let output = run("omarchy-shell", &["shell", "listPlugins"]).ok()?;
    let plugins: Vec<serde_json::Value> = serde_json::from_str(&output).ok()?;
    Some(plugins.iter().any(|plugin| {
        plugin["id"].as_str() == Some(ID) && plugin["enabled"].as_bool() == Some(true)
    }))
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = [stderr.trim(), stdout.trim()]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("failed");
        Err(format!("{program}: {message}"))
    }
}

/// Links the widget into the shell's plugin folder and enables it. Blocking.
pub fn add() -> Result<(), String> {
    let target = target();
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    if std::fs::symlink_metadata(&target).is_err() {
        std::os::unix::fs::symlink(SOURCE, &target).map_err(|e| e.to_string())?;
    }
    enable(run, std::thread::sleep)
}

fn enable(
    mut run: impl FnMut(&str, &[&str]) -> Result<String, String>,
    mut sleep: impl FnMut(Duration),
) -> Result<(), String> {
    run("omarchy-shell", &["shell", "rescanPlugins"])?;
    // rescanPlugins only starts the shell's asynchronous manifest scan. Wait
    // for the live registry (not the on-disk catalog) before trying to enable.
    for attempt in 0..50 {
        let output = run("omarchy-shell", &["shell", "listPlugins"])?;
        let plugins: Vec<serde_json::Value> = serde_json::from_str(&output)
            .map_err(|e| format!("Could not read the shell's plugin list: {e}"))?;
        if let Some(plugin) = plugins
            .iter()
            .find(|plugin| plugin["id"].as_str() == Some(ID))
        {
            // Already on the bar: enabling again would move it or fail.
            if plugin["enabled"].as_bool() != Some(true)
                && let Err(error) = run("omarchy", &["plugin", "enable", ID, "--section", "right"])
            {
                // Right after a rescan the shell can take longer to enable a
                // plugin than omarchy-shell waits for its answer, and puts it
                // on the bar anyway. What the shell reports is the answer.
                if !reached_the_bar(&mut run, &mut sleep) {
                    return Err(error);
                }
            }
            return Ok(());
        }
        if attempt < 49 {
            sleep(Duration::from_millis(100));
        }
    }
    Err(format!(
        "The shell did not discover {ID} after rescanning. Try reopening the app to add it again."
    ))
}

/// Whether the widget shows up on the bar within ten seconds or so. In #12 it
/// was there about four seconds after the enable gave up.
fn reached_the_bar(
    mut run: impl FnMut(&str, &[&str]) -> Result<String, String>,
    mut sleep: impl FnMut(Duration),
) -> bool {
    for attempt in 0..20 {
        if enabled(&mut run) == Some(true) {
            return true;
        }
        if attempt < 19 {
            sleep(Duration::from_millis(500));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Runs `enable` against canned replies; returns the calls and the time slept.
    fn scenario(replies: Vec<Result<&str, &str>>) -> (Result<(), String>, Vec<String>, Duration) {
        let mut replies: VecDeque<_> = replies.into();
        let mut calls = Vec::new();
        let mut slept = Duration::ZERO;
        let result = enable(
            |program, args| {
                calls.push(format!("{program} {}", args.join(" ")));
                replies
                    .pop_front()
                    .expect("unexpected command")
                    .map(str::to_owned)
                    .map_err(str::to_owned)
            },
            |duration| slept += duration,
        );
        assert!(replies.is_empty());
        (result, calls, slept)
    }

    const FOUND: &str = r#"[{"id":"jankeesvw.meeting-recorder","enabled":false}]"#;
    const ON_THE_BAR: &str = r#"[{"id":"jankeesvw.meeting-recorder","enabled":true}]"#;
    const NOT_RESPONDING: &str = "omarchy: omarchy-plugin-enable: omarchy-shell is not responding";
    const ENABLE: &str = "omarchy plugin enable jankeesvw.meeting-recorder --section right";

    #[test]
    fn an_enable_that_times_out_but_lands_counts_as_added() {
        let (result, calls, slept) = scenario(vec![
            Ok(""),
            Ok(FOUND),
            Err(NOT_RESPONDING),
            Err(NOT_RESPONDING),
            Ok(FOUND),
            Ok(ON_THE_BAR),
        ]);
        assert_eq!(result, Ok(()));
        assert_eq!(slept, Duration::from_secs(1));
        assert_eq!(calls.iter().filter(|call| *call == ENABLE).count(), 1);
    }

    #[test]
    fn an_enable_that_never_lands_keeps_its_error() {
        let mut replies = vec![Ok(""), Ok(FOUND), Err(NOT_RESPONDING)];
        replies.extend(vec![Ok(FOUND); 20]);
        let (result, calls, slept) = scenario(replies);
        assert_eq!(result, Err(NOT_RESPONDING.to_owned()));
        assert_eq!(slept, Duration::from_millis(9500));
        assert_eq!(calls.iter().filter(|call| *call == ENABLE).count(), 1);
    }

    #[test]
    fn a_widget_already_on_the_bar_is_left_alone() {
        let (result, calls, _) = scenario(vec![Ok(""), Ok(ON_THE_BAR)]);
        assert_eq!(result, Ok(()));
        assert!(
            calls
                .iter()
                .all(|call| !call.starts_with("omarchy plugin enable"))
        );
    }

    #[test]
    fn the_shell_says_whether_it_is_on_the_bar() {
        let answer = |text: &'static str| move |_: &str, _: &[&str]| Ok(text.to_owned());
        assert_eq!(enabled(&answer(ON_THE_BAR)), Some(true));
        assert_eq!(enabled(&answer(FOUND)), Some(false));
        assert_eq!(enabled(&answer("[]")), Some(false));
        assert_eq!(
            enabled(&|_: &str, _: &[&str]| Err("no shell".to_owned())),
            None
        );
    }

    #[test]
    fn waits_for_discovery_before_enabling() {
        let (result, calls, slept) = scenario(vec![
            Ok(""),
            Ok("[]"),
            Ok(r#"[{"id":"other.plugin"}]"#),
            Ok(FOUND),
            Ok("Enabled"),
        ]);
        assert_eq!(result, Ok(()));
        assert_eq!(slept, Duration::from_millis(200));
        assert_eq!(
            calls,
            [
                "omarchy-shell shell rescanPlugins",
                "omarchy-shell shell listPlugins",
                "omarchy-shell shell listPlugins",
                "omarchy-shell shell listPlugins",
                "omarchy plugin enable jankeesvw.meeting-recorder --section right",
            ]
        );
    }

    #[test]
    fn already_discovered_plugin_needs_no_sleep() {
        let (result, _, slept) = scenario(vec![Ok(""), Ok(FOUND), Ok("Enabled")]);
        assert_eq!(result, Ok(()));
        assert_eq!(slept, Duration::ZERO);
    }

    #[test]
    fn missing_plugin_times_out_without_enabling() {
        let mut replies = vec![Ok("")];
        replies.extend(vec![Ok("[]"); 50]);
        let (result, calls, slept) = scenario(replies);
        assert!(result.unwrap_err().contains("did not discover"));
        assert_eq!(slept, Duration::from_millis(4900));
        assert!(calls.iter().all(|call| call.starts_with("omarchy-shell ")));
    }

    #[test]
    fn command_failures_are_preserved() {
        for replies in [
            vec![Err("shell unavailable")],
            vec![Ok(""), Err("shell unavailable")],
        ] {
            let expected = replies.last().unwrap().as_ref().unwrap_err().to_string();
            let (result, _, slept) = scenario(replies);
            assert_eq!(result, Err(expected));
            assert_eq!(slept, Duration::ZERO);
        }
    }

    #[test]
    fn malformed_plugin_list_is_reported_without_enabling() {
        for output in ["not JSON", "{}"] {
            let (result, calls, slept) = scenario(vec![Ok(""), Ok(output)]);
            assert!(result.unwrap_err().contains("Could not read"));
            assert_eq!(calls.len(), 2);
            assert_eq!(slept, Duration::ZERO);
        }
    }
}
