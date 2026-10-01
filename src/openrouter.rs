//! Optional cloud transcription. Whisper remains the default; speakers stay local.
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::transcribe::{Abort, CANCELLED, Event, Events};

pub const MODEL: &str = "microsoft/mai-transcribe-2";

#[derive(Clone, Copy, PartialEq)]
pub enum Backend {
    Whisper,
    OpenRouter,
}
impl Backend {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "whisper" => Ok(Self::Whisper),
            "openrouter" | "mai" => Ok(Self::OpenRouter),
            _ => Err("backend must be whisper or openrouter".into()),
        }
    }
    /// A root-level config choice. Cloud transcription is opt-in.
    pub fn configured() -> Self {
        std::fs::read_to_string(crate::models::config_file())
            .map(|text| Self::from_config(&text))
            .unwrap_or(Self::Whisper)
    }

    fn from_config(text: &str) -> Self {
        toml::from_str::<toml::Value>(&text)
            .ok()
            .and_then(|config| {
                config
                    .get("backend")?
                    .as_str()
                    .and_then(|v| Self::parse(v).ok())
            })
            .unwrap_or(Self::Whisper)
    }

    /// Snapshot once per job: both tracks and the manifest use the same model.
    pub fn prepare(selected: Option<Self>) -> Result<Engine, String> {
        let config = match fs::read_to_string(crate::models::config_file()) {
            Ok(text) => toml::from_str::<toml::Value>(&text)
                .map_err(|_| "Invalid transcription config.toml".to_string())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                toml::Value::Table(Default::default())
            }
            Err(e) => return Err(format!("Could not read transcription config: {e}")),
        };
        Self::select(&config, selected)
    }

    fn select(config: &toml::Value, selected: Option<Self>) -> Result<Engine, String> {
        let backend = match selected {
            Some(backend) => backend,
            None => match config.get("backend") {
                None => Self::Whisper,
                Some(value) => Self::parse(value.as_str().ok_or("backend must be a string")?)?,
            },
        };
        backend.prepare_config(config)
    }

    fn prepare_config(self, config: &toml::Value) -> Result<Engine, String> {
        let options = match config.get("openrouter") {
            Some(value) if self == Self::OpenRouter => {
                serde_json::to_value(value).map_err(|e| e.to_string())?
            }
            _ => serde_json::json!({}),
        };
        let model = if self == Self::OpenRouter {
            options
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or(MODEL)
                .to_owned()
        } else {
            crate::models::selected(config)
        };
        Ok(Engine {
            backend: self,
            model,
            options,
        })
    }
}

pub struct Engine {
    pub backend: Backend,
    pub model: String,
    pub options: serde_json::Value,
}

pub struct TimedWord {
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
}
struct WorkDir(PathBuf);
impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// Bundle the adapter with the binary so release upgrades cannot mix versions.
const HELPER: &str = include_str!("../integrations/openrouter-transcribe.py");

pub fn transcribe(
    options: &serde_json::Value,
    samples: &[f32],
    language: &str,
    progress: (f64, f64),
    events: &Events,
    abort: &Abort,
) -> Result<(Vec<TimedWord>, Option<String>), String> {
    if abort.load(Ordering::Relaxed) {
        return Err(CANCELLED.into());
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = gtk::glib::user_cache_dir()
        .join(crate::APP_NAME)
        .join("openrouter-work");
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let dir = root.join(format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| e.to_string())?;
    let work = WorkDir(dir);
    let helper = work.0.join("openrouter-transcribe.py");
    fs::write(&helper, HELPER).map_err(|e| e.to_string())?;
    let input = work.0.join("audio.f32");
    let output = work.0.join("result.json");
    let settings = work.0.join("settings.json");
    fs::write(
        &settings,
        serde_json::to_vec(options).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let status = work.0.join("progress.json");
    let error = work.0.join("error.txt");
    {
        let mut file = BufWriter::new(File::create(&input).map_err(|e| e.to_string())?);
        for block in samples.chunks(16_000) {
            if abort.load(Ordering::Relaxed) {
                return Err(CANCELLED.into());
            }
            for sample in block {
                file.write_all(&sample.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
        }
        file.flush().map_err(|e| e.to_string())?;
    }
    let _ = events.send_blocking(Event::Stage("Sending audio to OpenRouter".into()));
    let mut child = Command::new("python3")
        .arg(helper)
        .arg("--settings")
        .arg(settings)
        .arg("--input")
        .arg(input)
        .arg("--output")
        .arg(&output)
        .arg("--progress")
        .arg(&status)
        .arg("--language")
        .arg(language)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(&error).map_err(|e| e.to_string())?)
        .process_group(0)
        .spawn()
        .map_err(|e| format!("could not start OpenRouter adapter: {e}"))?;
    let mut last = String::new();
    let success = loop {
        if abort.load(Ordering::Relaxed) {
            // Kill the isolated process group, including an in-flight ffmpeg conversion.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            return Err(CANCELLED.into());
        }
        if let Ok(text) = fs::read_to_string(&status)
            && text != last
        {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(stage) = value["stage"].as_str() {
                    let _ = events.send_blocking(Event::Stage(stage.into()));
                }
                if let Some(pct) = value["progress"].as_f64() {
                    let _ = events.send_blocking(Event::Progress(
                        progress.0 + (progress.1 - progress.0) * pct.clamp(0.0, 1.0),
                    ));
                }
            }
            last = text;
        }
        match child.try_wait() {
            Ok(Some(result)) => break result.success(),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.wait();
                return Err(e.to_string());
            }
        }
    };
    if !success {
        return Err(fs::read_to_string(error)
            .unwrap_or_else(|_| "OpenRouter transcription failed".into())
            .trim()
            .chars()
            .take(1000)
            .collect());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(output).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    parse_words(&value, (samples.len() as i64 * 1000) / 16_000)
}

fn parse_words(
    value: &serde_json::Value,
    duration_ms: i64,
) -> Result<(Vec<TimedWord>, Option<String>), String> {
    let mut words = Vec::new();
    let mut previous = -1;
    for word in value["words"]
        .as_array()
        .ok_or("OpenRouter adapter returned no words")?
    {
        let text = word["text"].as_str().ok_or("invalid OpenRouter word")?;
        let start = word["start_ms"]
            .as_i64()
            .ok_or("invalid OpenRouter timestamp")?;
        let end = word["end_ms"]
            .as_i64()
            .ok_or("invalid OpenRouter timestamp")?;
        if start < previous || start < 0 || end < start || end > duration_ms + 1 {
            return Err("invalid OpenRouter timestamp range".into());
        }
        previous = start;
        if !text.trim().is_empty() {
            words.push(TimedWord {
                text: text.into(),
                start_ms: start,
                end_ms: end,
            });
        }
    }
    Ok((words, value["language"].as_str().map(str::to_owned)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_job_keeps_model_file_and_alignment_after_config_changes() {
        let path = std::env::temp_dir().join(format!("recorder-model-{}.bin", std::process::id()));
        std::fs::File::create(&path)
            .unwrap()
            .set_len(10_000_000)
            .unwrap();
        let mut config = toml::Value::Table(Default::default());
        config.as_table_mut().unwrap().insert(
            "model".into(),
            toml::Value::String(path.to_str().unwrap().into()),
        );
        let engine = Backend::Whisper.prepare_config(&config).unwrap();
        config["model"] = toml::Value::String("small".into());
        let (events, _) = async_channel::unbounded();
        let loaded =
            crate::models::ensure_named(&engine.model, &events, &Abort::default()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(loaded, path);
        assert!(crate::models::dtw_preset(&engine.model).is_none());
        let config = toml::from_str::<toml::Value>("model='tiny'").unwrap();
        let engine = Backend::Whisper.prepare_config(&config).unwrap();
        assert!(matches!(
            crate::models::dtw_preset(&engine.model),
            Some(whisper_rs::DtwModelPreset::Tiny)
        ));
    }

    #[test]
    fn unknown_backend_is_an_error_unless_explicitly_overridden() {
        let config = toml::from_str::<toml::Value>("backend='openroutr'").unwrap();
        assert!(Backend::select(&config, None).is_err());
        assert!(Backend::select(&config, Some(Backend::Whisper)).is_ok());
    }

    #[test]
    fn prepared_job_keeps_its_model_and_options() {
        let mut config =
            toml::from_str::<toml::Value>("[openrouter]\nmodel='first/stt'\nchunk_seconds=10")
                .unwrap();
        let engine = Backend::OpenRouter.prepare_config(&config).unwrap();
        config["openrouter"]["model"] = toml::Value::String("second/stt".into());
        assert_eq!(engine.model, "first/stt");
        assert_eq!(engine.options["model"], "first/stt");
        assert_eq!(engine.options["chunk_seconds"], 10);
    }

    #[test]
    fn backend_requires_explicit_root_config() {
        for text in [
            "",
            "# backend = \"mai\"",
            "backend = \"unknown\"",
            "backend = mai",
            "[openrouter]\nbackend = \"mai\"",
            "[[action]]\nbackend = \"mai\"",
        ] {
            assert!(Backend::from_config(text) == Backend::Whisper);
        }
        for text in [
            "backend = \"openrouter\"",
            "backend = 'mai' # cloud\n[openrouter]",
            "# comment\nbackend = \"mai\"\n[[action]]",
        ] {
            assert!(Backend::from_config(text) == Backend::OpenRouter);
        }
        assert!(Backend::from_config("backend = \"whisper\"") == Backend::Whisper);
    }

    #[test]
    fn pre_cancelled_work_does_not_start_the_adapter() {
        let (events, _) = async_channel::unbounded();
        let abort = Abort::default();
        abort.store(true, Ordering::Relaxed);
        let result = transcribe(
            &serde_json::json!({}),
            &[0.0; 16],
            "no",
            (0.0, 1.0),
            &events,
            &abort,
        );
        assert!(matches!(result, Err(message) if message == CANCELLED));
    }

    #[test]
    fn rejects_invalid_timeline_and_accepts_real_words() {
        let valid = serde_json::json!({"words":[{"text":"Hei", "start_ms":100, "end_ms":400}], "language":"no"});
        assert_eq!(parse_words(&valid, 1000).unwrap().0[0].text, "Hei");
        assert!(parse_words(&valid, 200).is_err());
        assert!(
            parse_words(
                &serde_json::json!({"words":[{"text":"x", "start_ms":-1,"end_ms":2}]}),
                10
            )
            .is_err()
        );
    }
}
