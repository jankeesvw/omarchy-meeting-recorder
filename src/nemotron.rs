//! NVIDIA's Nemotron 3 Diarization, offline, through ONNX Runtime.
//!
//! The model is a Streaming Sortformer: a transformer that looks at a chunk of
//! audio together with a small memory of earlier frames (the speaker cache and
//! a FIFO of the latest ones), and gives every 10 ms frame a probability for
//! each of up to eight speakers, numbered in the order they are first heard.
//!
//! The network is the int8 ONNX export from the Hugging Face ONNX community,
//! which holds no state: it takes the log-mel features of one chunk and the
//! cached frames, and returns the logits and the chunk's frames for the cache.
//! The features, the chunk loop and the cache policy are here, and follow
//! `Nemotron3DiarizationSpeakerCache` in Hugging Face transformers step by step.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use ort::session::Session;
use ort::value::Tensor;
use realfft::RealFftPlanner;

use crate::transcribe::{Abort, CANCELLED, Event, Events, download, models_dir};

/// A fixed revision, so a later change upstream never reaches the app unseen.
const REPO: &str = "https://huggingface.co/onnx-community/Nemotron-3-Diarization-ONNX/resolve/353b6f8ad2cac3580e982d7fbdf0a010786b0406/onnx";
/// The graph, and its weights next to it under the name the graph refers to.
const FILES: [(&str, u64); 2] = [
    ("model_quantized.onnx", 300_000),
    ("model_quantized.onnx_data", 120_000_000),
];

const HOP: usize = 160;
const N_FFT: usize = 512;
const WIN: usize = 400;
const BINS: usize = N_FFT / 2 + 1;
const MELS: usize = 128;
const RATE: f64 = 16_000.0;
const PREEMPHASIS: f32 = 0.97;

/// The model's streaming config (`config.json`), offline sizes.
struct Config {
    hidden_size: usize,
    subsampling_factor: usize,
    num_speakers: usize,
    chunk_length: usize,
    chunk_right_context: usize,
    fifo_length: usize,
    speaker_cache_update_period: usize,
    speaker_cache_length: usize,
    silence_frames_per_speaker: usize,
    prediction_score_threshold: f32,
    latest_frames_score_boost: f32,
    min_positive_scores_rate: f32,
    strong_boost_rate: f32,
    weak_boost_rate: f32,
    /// Learned; the model returns it with every step.
    silence_embeds: Vec<f32>,
}

const CONFIG: Config = Config {
    hidden_size: 512,
    subsampling_factor: 8,
    num_speakers: 8,
    chunk_length: 340,
    chunk_right_context: 40,
    fifo_length: 40,
    speaker_cache_update_period: 300,
    speaker_cache_length: 264,
    silence_frames_per_speaker: 1,
    prediction_score_threshold: 0.25,
    latest_frames_score_boost: 0.05,
    min_positive_scores_rate: 0.5,
    strong_boost_rate: 0.75,
    weak_boost_rate: 1.5,
    silence_embeds: Vec::new(),
};

fn dir() -> PathBuf {
    models_dir().join("nemotron-3-diarization")
}

/// The model files, downloaded first when needed (about 120 MB).
pub fn ensure(events: &Events, abort: &Abort) -> Result<PathBuf, String> {
    let dir = dir();
    for (file, min_bytes) in FILES {
        let path = dir.join(file);
        if std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() >= min_bytes) {
            continue;
        }
        download(
            &format!("{REPO}/{file}"),
            &path,
            "Downloading the speaker model",
            min_bytes,
            events,
            abort,
        )?;
    }
    Ok(dir.join(FILES[0].0))
}

pub struct Model {
    session: Session,
    config: Config,
}

fn ort_error(e: impl std::fmt::Display) -> String {
    format!("speaker model: {e}")
}

impl Model {
    pub fn load(path: &Path) -> Result<Self, String> {
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(8);
        let session = Session::builder()
            .map_err(ort_error)?
            .with_intra_threads(threads)
            .map_err(ort_error)?
            .commit_from_file(path)
            .map_err(ort_error)?;
        Ok(Self {
            session,
            config: CONFIG,
        })
    }

    /// Speaker probabilities for every 10 ms of `samples` (16 kHz mono):
    /// `frames x num_speakers`, row-major.
    pub fn probabilities(
        &mut self,
        samples: &[f32],
        events: &Events,
        abort: &Abort,
    ) -> Result<Vec<f32>, String> {
        let spectrum = Spectrum::new(samples);
        let mel = MelFilters::new();
        let factor = self.config.subsampling_factor;
        let n = self.config.num_speakers;
        let h = self.config.hidden_size;
        let frames = spectrum.frames;
        let valid = samples.len() / HOP;
        let steps = frames.div_ceil(factor);

        let mut cache = Cache::new();
        let mut logits = Vec::with_capacity(steps * factor * n);
        let mut start = 0;
        while start < steps {
            if abort.load(Ordering::Relaxed) {
                return Err(CANCELLED.into());
            }
            let end = (start + self.config.chunk_length).min(steps);
            let chunk_steps = end - start;
            let with_lookahead = (end + self.config.chunk_right_context).min(steps);
            let rows = (with_lookahead - start) * factor;
            let first = start * factor;
            let features = mel.log_mel(&spectrum, first, (first + rows).min(frames), valid, rows);

            let cached = cache.embeds();
            let cached_len = cached.len() / h;
            let total = cached_len + with_lookahead - start;
            let outputs = self
                .session
                .run(ort::inputs![
                    "input_features" => Tensor::from_array(([1usize, rows, MELS], features)).map_err(ort_error)?,
                    "cached_embeds" => Tensor::from_array(([1usize, cached_len, h], cached.clone())).map_err(ort_error)?,
                    "attention_mask" => Tensor::from_array(([1usize, total], vec![1i64; total])).map_err(ort_error)?,
                ])
                .map_err(ort_error)?;
            let (_, step_logits) = outputs["logits"]
                .try_extract_tensor::<f32>()
                .map_err(ort_error)?;
            let (_, chunk_embeds) = outputs["chunk_embeds"]
                .try_extract_tensor::<f32>()
                .map_err(ort_error)?;
            if self.config.silence_embeds.is_empty() {
                let (_, silence) = outputs["silence_embeds"]
                    .try_extract_tensor::<f32>()
                    .map_err(ort_error)?;
                self.config.silence_embeds = silence.to_vec();
            }
            logits.extend_from_slice(
                &step_logits[cached_len * factor * n..(cached_len + chunk_steps) * factor * n],
            );
            let mut input = cached;
            input.extend_from_slice(chunk_embeds);
            cache.update(&self.config, &input, step_logits, chunk_steps);
            let _ = events.send_blocking(Event::Progress(end as f64 / steps as f64));
            start = end;
        }
        Ok(logits[..(frames * n).min(logits.len())]
            .iter()
            .map(|l| 1.0 / (1.0 + (-l).exp()))
            .collect())
    }
}

/// librosa's Slaney mel filterbank (`librosa.filters.mel(norm="slaney")`),
/// 257 FFT bins to 128 mel bands from 0 to 8 kHz.
struct MelFilters {
    /// Per band, the first FFT bin it covers and its weights from there on:
    /// a band is a narrow triangle, so most of its 257 weights are zero.
    bands: Vec<(usize, Vec<f32>)>,
}

impl MelFilters {
    fn new() -> Self {
        fn hz_to_mel(hz: f64) -> f64 {
            let (f_sp, min_log_hz) = (200.0 / 3.0, 1000.0);
            let logstep = 6.4f64.ln() / 27.0;
            if hz >= min_log_hz {
                min_log_hz / f_sp + (hz / min_log_hz).ln() / logstep
            } else {
                hz / f_sp
            }
        }
        fn mel_to_hz(mel: f64) -> f64 {
            let (f_sp, min_log_hz) = (200.0 / 3.0, 1000.0);
            let min_log_mel = min_log_hz / f_sp;
            let logstep = 6.4f64.ln() / 27.0;
            if mel >= min_log_mel {
                min_log_hz * (logstep * (mel - min_log_mel)).exp()
            } else {
                f_sp * mel
            }
        }
        let top = hz_to_mel(RATE / 2.0);
        let points: Vec<f64> = (0..MELS + 2)
            .map(|i| mel_to_hz(top * i as f64 / (MELS + 1) as f64))
            .collect();
        let fft_hz: Vec<f64> = (0..BINS)
            .map(|k| k as f64 * RATE / 2.0 / (BINS - 1) as f64)
            .collect();
        let mut weights = vec![0.0f32; MELS * BINS];
        for m in 0..MELS {
            let (lower, center, upper) = (points[m], points[m + 1], points[m + 2]);
            let norm = 2.0 / (upper - lower);
            for (k, hz) in fft_hz.iter().enumerate() {
                let rising = (hz - lower) / (center - lower);
                let falling = (upper - hz) / (upper - center);
                weights[m * BINS + k] = (rising.min(falling).max(0.0) * norm) as f32;
            }
        }
        let bands = weights
            .chunks(BINS)
            .map(|row| {
                let first = row.iter().position(|w| *w > 0.0).unwrap_or(0);
                let last = row.iter().rposition(|w| *w > 0.0).unwrap_or(0);
                (first, row[first..=last.max(first)].to_vec())
            })
            .collect();
        Self { bands }
    }

    /// `log(mel + 2^-24)` of spectrum frames `from..to`, zero from frame
    /// `valid` on, padded with zero rows to `rows`: `rows x MELS`.
    fn log_mel(
        &self,
        spectrum: &Spectrum,
        from: usize,
        to: usize,
        valid: usize,
        rows: usize,
    ) -> Vec<f32> {
        let power = spectrum.power(from, to);
        let mut out = vec![0.0f32; rows * MELS];
        for f in 0..to - from {
            if from + f >= valid {
                break;
            }
            let bins = &power[f * BINS..(f + 1) * BINS];
            for (m, (first, w)) in self.bands.iter().enumerate() {
                let energy: f32 = w.iter().zip(&bins[*first..]).map(|(a, b)| a * b).sum();
                out[f * MELS + m] = (energy + 2f32.powi(-24)).ln();
            }
        }
        out
    }
}

/// Pre-emphasis, then `torch.stft(center=True)` with a 400-sample symmetric
/// Hann window centred in 512, as squared magnitudes, computed a block of
/// frames at a time straight from the samples.
struct Spectrum<'a> {
    samples: &'a [f32],
    window: Vec<f32>,
    frames: usize,
}

impl<'a> Spectrum<'a> {
    fn new(samples: &'a [f32]) -> Self {
        let offset = (N_FFT - WIN) / 2;
        let window = (0..N_FFT)
            .map(|i| {
                if i < offset || i >= offset + WIN {
                    0.0
                } else {
                    let k = (i - offset) as f32;
                    0.5 - 0.5 * (2.0 * std::f32::consts::PI * k / (WIN - 1) as f32).cos()
                }
            })
            .collect();
        Self {
            samples,
            window,
            frames: 1 + samples.len() / HOP,
        }
    }

    /// Sample `i` of the pre-emphasised signal, padded by half a window on
    /// both sides.
    fn at(&self, i: usize) -> f32 {
        let Some(j) = i.checked_sub(N_FFT / 2).filter(|j| *j < self.samples.len()) else {
            return 0.0;
        };
        match j {
            0 => self.samples[0],
            _ => self.samples[j] - PREEMPHASIS * self.samples[j - 1],
        }
    }

    /// Frames `start..end`: `(end - start) x 257`.
    fn power(&self, start: usize, end: usize) -> Vec<f32> {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(N_FFT);
        let mut input = fft.make_input_vec();
        let mut spectrum = fft.make_output_vec();
        let mut power = Vec::with_capacity((end - start) * BINS);
        for f in start..end {
            let at = f * HOP;
            for (i, v) in input.iter_mut().enumerate() {
                *v = self.at(at + i) * self.window[i];
            }
            fft.process(&mut input, &mut spectrum)
                .expect("buffers from the plan");
            power.extend(spectrum.iter().map(|c| c.norm_sqr()));
        }
        power
    }
}

/// The Arrival-Order Speaker Cache and FIFO, rows of `hidden_size`.
struct Cache {
    cache_embeds: Vec<f32>,
    cache_probs: Vec<f32>,
    fifo: Vec<f32>,
    compressed: bool,
}

impl Cache {
    fn new() -> Self {
        Self {
            cache_embeds: Vec::new(),
            cache_probs: Vec::new(),
            fifo: Vec::new(),
            compressed: false,
        }
    }

    fn embeds(&self) -> Vec<f32> {
        let mut all = self.cache_embeds.clone();
        all.extend_from_slice(&self.fifo);
        all
    }

    /// Pushes a processed chunk to the FIFO, moving its oldest frames to the
    /// speaker cache when it overflows, and compressing the cache when that
    /// outgrows its length.
    fn update(&mut self, c: &Config, input: &[f32], logits: &[f32], chunk_frames: usize) {
        let (h, n) = (c.hidden_size, c.num_speakers);
        let cache_len = self.cache_embeds.len() / h;
        let fifo_len = self.fifo.len() / h;
        let probs = pool_probs(logits, c.subsampling_factor, n);

        let chunk_start = cache_len + fifo_len;
        let mut fifo = self.fifo.clone();
        fifo.extend_from_slice(&input[chunk_start * h..(chunk_start + chunk_frames) * h]);
        let fifo_rows = fifo.len() / h;

        let popped = if fifo_rows <= c.fifo_length {
            0
        } else {
            c.speaker_cache_update_period
                .max(fifo_rows - c.fifo_length)
                .min(fifo_rows)
        };
        if popped > 0 {
            let fifo_probs = &probs[cache_len * n..(cache_len + fifo_rows) * n];
            let mut cache_probs = if self.compressed {
                self.cache_probs[..cache_len * n].to_vec()
            } else {
                probs[..cache_len * n].to_vec()
            };
            let mut cache_embeds = self.cache_embeds.clone();
            cache_embeds.extend_from_slice(&fifo[..popped * h]);
            cache_probs.extend_from_slice(&fifo_probs[..popped * n]);
            fifo.drain(..popped * h);
            if cache_embeds.len() / h > c.speaker_cache_length {
                (cache_embeds, cache_probs) = Self::compress(c, &cache_embeds, &cache_probs);
                self.compressed = true;
            }
            self.cache_embeds = cache_embeds;
            self.cache_probs = cache_probs;
        }
        self.fifo = fifo;
    }

    /// Frame scores for the cache: high for frames that clearly belong to one
    /// speaker; -inf for frames that are not that speaker's speech.
    fn frame_scores(c: &Config, probs: &[f32], frames: usize) -> Vec<f32> {
        let n = c.num_speakers;
        let budget = c.speaker_cache_length / n - c.silence_frames_per_speaker;
        let min_positive = (budget as f32 * c.min_positive_scores_rate).floor() as usize;
        let t = c.prediction_score_threshold;
        let mut scores = vec![0.0f32; frames * n];
        for f in 0..frames {
            let row = &probs[f * n..(f + 1) * n];
            let complements: Vec<f32> = row.iter().map(|p| (1.0 - p).max(t).ln()).collect();
            let sum: f32 = complements.iter().sum();
            for s in 0..n {
                let p = row[s];
                scores[f * n + s] = if p > 0.5 {
                    p.max(t).ln() - complements[s] + sum - 0.5f32.ln()
                } else {
                    f32::NEG_INFINITY
                };
            }
        }
        for s in 0..n {
            let positive = (0..frames).filter(|f| scores[f * n + s] > 0.0).count();
            if positive >= min_positive {
                for f in 0..frames {
                    let v = &mut scores[f * n + s];
                    if *v != f32::NEG_INFINITY && *v <= 0.0 {
                        *v = f32::NEG_INFINITY;
                    }
                }
            }
        }
        scores
    }

    /// Keeps the `speaker_cache_length` most telling frames, grouped by
    /// speaker and in their original order within a speaker, with one slot of
    /// learned silence per speaker.
    fn compress(c: &Config, embeds: &[f32], probs: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let (h, n) = (c.hidden_size, c.num_speakers);
        let frames = embeds.len() / h;
        let mut scores = Self::frame_scores(c, probs, frames);
        for f in c.speaker_cache_length..frames {
            for s in 0..n {
                scores[f * n + s] += c.latest_frames_score_boost;
            }
        }
        let budget = c.speaker_cache_length / n - c.silence_frames_per_speaker;
        let strong = (budget as f32 * c.strong_boost_rate).floor() as usize;
        let weak = (budget as f32 * c.weak_boost_rate).floor() as usize;
        boost(&mut scores, frames, n, strong, -2.0 * 0.5f32.ln());
        boost(&mut scores, frames, n, weak, -(0.5f32.ln()));

        // Speaker-major flat index over frames plus the silence slots.
        let silence = c.silence_frames_per_speaker;
        let scored = frames + silence;
        let mut flat: Vec<(f32, usize)> = Vec::with_capacity(scored * n);
        for s in 0..n {
            for f in 0..scored {
                let v = if f < frames {
                    scores[f * n + s]
                } else {
                    f32::INFINITY
                };
                flat.push((v, s * scored + f));
            }
        }
        flat.sort_by(|a, b| b.0.total_cmp(&a.0));
        let sentinel = scored * n;
        let mut picked: Vec<usize> = flat[..c.speaker_cache_length]
            .iter()
            .map(|(v, i)| {
                if *v == f32::NEG_INFINITY {
                    sentinel
                } else {
                    *i
                }
            })
            .collect();
        picked.sort_unstable();

        let mut out_embeds = Vec::with_capacity(c.speaker_cache_length * h);
        let mut out_probs = Vec::with_capacity(c.speaker_cache_length * n);
        for i in picked {
            let frame = if i == sentinel {
                frames
            } else {
                (i % scored).min(frames)
            };
            if frame < frames {
                out_embeds.extend_from_slice(&embeds[frame * h..(frame + 1) * h]);
                out_probs.extend_from_slice(&probs[frame * n..(frame + 1) * n]);
            } else {
                out_embeds.extend_from_slice(&c.silence_embeds);
                out_probs.extend(std::iter::repeat_n(0.0, n));
            }
        }
        (out_embeds, out_probs)
    }
}

/// Adds `amount` to the `count` highest scores of every speaker.
fn boost(scores: &mut [f32], frames: usize, n: usize, count: usize, amount: f32) {
    let count = count.min(frames);
    for s in 0..n {
        let mut order: Vec<usize> = (0..frames).collect();
        order.sort_by(|a, b| scores[b * n + s].total_cmp(&scores[a * n + s]));
        for f in &order[..count] {
            scores[f * n + s] += amount;
        }
    }
}

/// Sigmoid of the 10 ms logits, averaged per encoder step: `steps x n`.
fn pool_probs(logits: &[f32], factor: usize, n: usize) -> Vec<f32> {
    let steps = logits.len() / (factor * n);
    let mut probs = vec![0.0f32; steps * n];
    for step in 0..steps {
        for k in 0..factor {
            let row = (step * factor + k) * n;
            for s in 0..n {
                probs[step * n + s] += 1.0 / (1.0 + (-logits[row + s]).exp()) / factor as f32;
            }
        }
    }
    probs
}
