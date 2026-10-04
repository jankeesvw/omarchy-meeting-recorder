//! Live captions: an optional, off-by-default preview of what is said while
//! you record. A low-priority background thread mixes the last few seconds
//! of both tracks the same way the final pass does, skips it when there is
//! only silence in it, and decodes it with a warm whisper context that has
//! no memory of the previous window. It only ever shows the last line or
//! two on screen; the transcript you get after you stop is made afterwards,
//! from the whole recording, and never touches this.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

use crate::audio::Source;
use crate::transcribe::{is_silent, mix, raw_to_whisper_rate};

/// How much recent audio each caption is decoded from.
const WINDOW_SECS: u32 = 5;
/// How long to wait before taking the next window. Decoding a window takes
/// a moment itself, so a slow machine naturally skips the ones in between
/// rather than queuing them up.
const INTERVAL: Duration = Duration::from_millis(1500);
/// Kept low so captioning never competes with the recorder, or the final
/// transcription once you stop, for the CPU.
const THREADS: i32 = 2;

/// A running caption session. Dropping it tells the background thread to
/// finish whatever it is doing and stop; nothing else needs to wait for it.
pub struct Captions {
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
}

impl Captions {
    /// Starts captioning `mic` and `system`, sending each recognised line to
    /// `on_line` from the background thread. `Err` gives a short reason the
    /// UI can show instead of starting: the speech model is looked for, but
    /// never downloaded, so captions stay quietly off until you fetch it
    /// yourself for the real transcript.
    pub fn start(
        mic: Source,
        system: Source,
        language: &'static str,
        on_line: impl Fn(String) + Send + 'static,
    ) -> Result<Self, &'static str> {
        let Some(model_path) = crate::models::find() else {
            return Err("Live captions need the speech model; download it above first.");
        };
        let running = Arc::new(AtomicBool::new(true));
        let paused = Arc::new(AtomicBool::new(false));
        let (flag, pause_flag) = (running.clone(), paused.clone());
        thread::spawn(move || run(mic, system, model_path, language, flag, pause_flag, on_line));
        Ok(Captions { running, paused })
    }

    /// Pausing the recording pauses captions too; resuming picks it back up.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }
}

impl Drop for Captions {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

fn run(
    mic: Source,
    system: Source,
    model_path: PathBuf,
    language: &str,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    on_line: impl Fn(String),
) {
    whisper_rs::install_logging_hooks();
    let mut context_params = WhisperContextParameters::default();
    context_params.use_gpu(cfg!(feature = "vulkan"));
    let Ok(context) = WhisperContext::new_with_params(&model_path, context_params) else {
        return;
    };
    let Ok(mut state) = context.create_state() else {
        return;
    };
    while running.load(Ordering::Relaxed) {
        thread::sleep(INTERVAL);
        if !running.load(Ordering::Relaxed) || paused.load(Ordering::Relaxed) {
            continue;
        }
        let window = mixed_window(&mic, &system);
        if is_silent(&window) {
            continue;
        }
        if let Some(text) = decode(&mut state, &window, language) {
            on_line(text);
        }
    }
}

/// The last `WINDOW_SECS` of both tracks, mixed the same way the final
/// transcript is: each brought to a similar speaking level first, so a
/// quiet mic is not drowned by loud computer audio.
fn mixed_window(mic: &Source, system: &Source) -> Vec<f32> {
    let mic = raw_to_whisper_rate(&mic.recent_raw(WINDOW_SECS));
    let system = raw_to_whisper_rate(&system.recent_raw(WINDOW_SECS));
    mix(&mic, &system)
}

/// One window through whisper: no context from the last one, one segment,
/// nothing printed. `None` when the window turned out to hold no speech.
fn decode(state: &mut WhisperState, samples: &[f32], language: &str) -> Option<String> {
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(THREADS);
    params.set_language(Some(language));
    params.set_no_context(true);
    params.set_single_segment(true);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_suppress_blank(true);
    params.set_suppress_nst(true);
    params.set_no_speech_thold(0.6);
    state.full(params, samples).ok()?;

    let mut text = String::new();
    for segment in state.as_iter() {
        if segment.no_speech_probability() > 0.6 {
            continue;
        }
        if let Ok(part) = segment.to_str_lossy() {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(part.trim());
        }
    }
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}
