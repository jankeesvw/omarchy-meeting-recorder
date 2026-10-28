//! Make a shortened copy, never modifying the original meeting.
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn copy_range(dir: &Path, start_us: i64, until_us: i64, label: &str) -> Result<PathBuf, String> {
    if start_us < 0 || until_us <= start_us {
        return Err("Choose a cut point after the start of the recording".into());
    }
    let (_, mut manifest) = crate::meeting::open(dir).ok_or("Cannot open meeting")?;
    let parent = dir.parent().ok_or("Meeting has no parent folder")?;
    let name = dir
        .file_name()
        .ok_or("Meeting has no folder name")?
        .to_string_lossy();
    let mut n = 1;
    let out = loop {
        let candidate = parent.join(format!("{name} ({label} {n})"));
        match std::fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e.to_string()),
        }
    };
    let result = (|| {
        let mut count = 0;
        for file in [
            "audio.ogg",
            "mic.ogg",
            "computer.ogg",
            ".tracks/mic.ogg",
            ".tracks/computer.ogg",
            ".tracks/source.ogg",
            "webcam.mp4",
        ] {
            let source = dir.join(file);
            if !source.is_file() {
                continue;
            }
            let target = out.join(file);
            std::fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
            let mut cmd = Command::new("ffmpeg");
            cmd.args(["-v", "error", "-nostdin"]);
            let start = format!("{:.6}", start_us as f64 / 1_000_000.0);
            // Ogg output seeking avoids copying the start of the whole page.
            // MP4 input seeking keeps decoder preroll through its edit list.
            if file.ends_with(".mp4") { cmd.args(["-ss", &start]); }
            cmd.arg("-i").arg(&source);
            if file.ends_with(".ogg") { cmd.args(["-ss", &start]); }
            cmd.args(["-t", &format!("{:.6}", (until_us - start_us) as f64 / 1_000_000.0), "-c", "copy"]);
            let output = cmd.arg(&target).output().map_err(|e| e.to_string())?;
            if !output.status.success() {
                return Err(format!(
                    "Could not trim {file}: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            count += 1;
        }
        if count == 0 {
            return Err("No recording files found".into());
        }
        manifest.title = format!("{} ({label})", manifest.title);
        manifest.started_at += start_us / 1_000_000;
        manifest.duration_secs = (until_us - start_us) / 1_000_000;
        manifest.chapters.retain(|chapter| chapter.start_ms >= start_us / 1000 && chapter.start_ms < until_us / 1000);
        for chapter in &mut manifest.chapters { chapter.start_ms -= start_us / 1000; }
        let transcript = dir.join("transcript.md");
        if transcript.is_file() {
            let text = std::fs::read_to_string(transcript).map_err(|e| e.to_string())?;
            let text = trim_transcript(&text, start_us / 1000, until_us / 1000);
            let text = crate::chapters::apply_to_markdown(&text, &manifest.chapters);
            std::fs::write(out.join("transcript.md"), text).map_err(|e| e.to_string())?;
        }
        let notes = dir.join(crate::notes::NOTES_FILE);
        if notes.is_file() {
            std::fs::copy(notes, out.join(crate::notes::NOTES_FILE)).map_err(|e| e.to_string())?;
        }
        crate::meeting::write(&out, &manifest).map_err(|e| e.to_string())?;
        Ok(out.clone())
    })();
    if result.is_err() {
        // Only this newly created copy is removed on failure.
        let _ = std::fs::remove_dir_all(&out);
    }
    result
}

fn trim_transcript(text: &str, start_ms: i64, until_ms: i64) -> String {
    let mut kept = String::new();
    let mut keep = true;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("**[")
            && let Some((time, _)) = rest.split_once("] ")
        {
            let seconds = time.split(':').try_fold(0i64, |total, part| {
                total.checked_mul(60)?.checked_add(part.parse::<i64>().ok()?)
            });
            keep = seconds.is_some_and(|seconds| seconds.saturating_mul(1000) >= start_ms && seconds.saturating_mul(1000) < until_ms);
            if keep {
                let shifted = crate::chapters::clock(seconds.unwrap() * 1000 - start_ms);
                kept.push_str(&line.replacen(&format!("**[{time}]"), &format!("**[{shifted}]"), 1));
                kept.push('\n');
                continue;
            }
        }
        if line.starts_with("- **Duration:** ") {
            kept.push_str(&format!("- **Duration:** {}\n", crate::chapters::clock(until_ms - start_ms)));
        } else if keep {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_playback_without_source_tracks_and_keeps_audio_cut_aligned() {
        // FFmpeg is an application runtime dependency. A chirp lets us measure
        // the copied audio's offset against a precisely decoded reference.
        let root = std::env::temp_dir().join(format!("meeting-trim-test-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let dir = root.join("Original");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("Original.meeting-recorder"), r#"{"title":"Original","started_at":0,"duration_secs":4,"format":"separate"}"#).unwrap();
        for file in ["mic.ogg", "computer.ogg"] {
            assert!(Command::new("ffmpeg").args(["-v", "error", "-f", "lavfi", "-i", "aevalsrc=0.5*sin(2*PI*(200*t+100*t*t)):s=48000:d=4", "-c:a", "libopus"]).arg(dir.join(file)).status().unwrap().success());
        }
        assert!(Command::new("ffmpeg").args(["-v", "error", "-f", "lavfi", "-i", "testsrc2=size=64x64:rate=20:duration=4", "-c:v", "libx264", "-g", "80"]).arg(dir.join("webcam.mp4")).status().unwrap().success());
        let out = copy_range(&dir, 1_350_000, 2_350_000, "selected").unwrap();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        assert!(!out.join(".tracks").exists());
        let (_, manifest) = crate::meeting::open(&out).unwrap();
        assert!(manifest.imported.is_none());
        let decoded = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(out.join("mic.ogg")).args(["-f", "f32le", "-ac", "1", "-ar", "48000", "-"]).output().unwrap();
        assert!(decoded.status.success());
        let samples: Vec<f32> = decoded.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        assert!((samples.len() as f64 / 48000.0 - 1.0).abs() < 0.05);
        let reference = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(dir.join("mic.ogg")).args(["-ss", "1.35", "-t", "1", "-f", "f32le", "-ac", "1", "-ar", "48000", "-"]).output().unwrap();
        assert!(reference.status.success());
        assert!((reference.stdout.len() as i64 - decoded.stdout.len() as i64).abs() < 48000 * 4 / 20);
        let reference: Vec<f32> = reference.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        let offset = (-1200i32..=1200).min_by(|a, b| {
            let error = |shift: i32| (2400..12000).step_by(8).map(|i| {
                let delta = samples[(i as i32 + shift) as usize] - reference[i];
                f64::from(delta * delta)
            }).sum::<f64>();
            error(*a).total_cmp(&error(*b))
        }).unwrap();
        assert!(offset.abs() <= 960, "Audio offset exceeds 20 ms: {offset} samples");
        let frame = |path: PathBuf, seek: &str| Command::new("ffmpeg")
            .args(["-v", "error", "-i"]).arg(path)
            .args(["-ss", seek, "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .output().unwrap();
        let original_frame = frame(dir.join("webcam.mp4"), "1.35");
        let copied_frame = frame(out.join("webcam.mp4"), "0");
        assert!(original_frame.status.success() && copied_frame.status.success());
        assert!(!copied_frame.stdout.is_empty());
        assert_eq!(original_frame.stdout, copied_frame.stdout, "MP4 edit list must present the selected frame, not the earlier keyframe");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn range_rebases_timestamps_and_excludes_both_ends() {
        let text = "## Transcript\n\n**[00:05] You:** Before\n**[00:10] You:** Selected\n**[00:20] You:** After\n";
        let result = trim_transcript(text, 10_000, 20_000);
        assert!(result.contains("**[00:00] You:** Selected"));
        assert!(!result.contains("Before"));
        assert!(!result.contains("After"));
    }

    #[test]
    fn trims_entries_at_or_after_cut_and_their_continuations() {
        let text = "- **Duration:** 3:00:00\n\n## Transcript\n\n**[59:58] You:** Keep\ncontinued\n\n**[1:00:00] Remote:** Remove\ncontinued too\n";
        let result = trim_transcript(text, 0, 3_600_000);
        assert!(result.contains("- **Duration:** 1:00:00"));
        assert!(result.contains("Keep\ncontinued"));
        assert!(!result.contains("Remove"));
        assert!(!result.contains("continued too"));
    }

    #[test]
    fn rejects_empty_or_negative_cut_before_creating_a_copy() {
        for cut in [0, -1] {
            assert!(
                copy_range(Path::new("/nonexistent/meeting"), 0, cut, "selected")
                    .unwrap_err()
                    .contains("cut point")
            );
        }
    }

    #[test]
    fn rejects_missing_meeting() {
        assert!(copy_range(Path::new("/nonexistent/meeting"), 0, 1_000_000, "selected").is_err());
    }
}
