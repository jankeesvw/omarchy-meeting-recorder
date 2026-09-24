//! NVIDIA's Nemotron 3 Diarization, offline, through ONNX Runtime.
//!
//! The model is a Streaming Sortformer: a transformer that looks at a chunk of
//! audio together with a small memory of earlier frames (the speaker cache and
//! a FIFO of the latest ones), and gives every 10 ms frame a probability for
//! each of up to eight speakers, numbered in the order they are first heard.
//! The network itself is stateless and exported as two ONNX graphs; the chunk
//! loop and the cache policy below follow `Nemotron3DiarizationSpeakerCache` in
//! Hugging Face transformers step by step.
//!
//! - `frontend.onnx`: power spectrum -> log-mel -> stacked encoder input
//! - `step.onnx`: encoder input of one step -> speaker logits per 10 ms frame
//! - `nemotron.json`: the cache sizes and the learned silence embedding

use std::path::Path;
use std::sync::atomic::Ordering;

use ort::session::Session;
use ort::value::Tensor;
use realfft::RealFftPlanner;

use crate::transcribe::{Abort, CANCELLED, Event, Events};

const HOP: usize = 160;
const N_FFT: usize = 512;
const WIN: usize = 400;
const BINS: usize = N_FFT / 2 + 1;
const PREEMPHASIS: f32 = 0.97;
/// Spectrum frames per frontend call, a multiple of the stacking factor; keeps
/// the power spectrum of a long meeting out of memory all at once.
const FRONTEND_BLOCK: usize = 8 * 4096;

#[derive(serde::Deserialize)]
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
    silence_embeds: Vec<f32>,
}

pub struct Model {
    frontend: Session,
    step: Session,
    config: Config,
}

fn ort_error(e: impl std::fmt::Display) -> String {
    format!("speaker model: {e}")
}

impl Model {
    pub fn load(dir: &Path, step_file: &str) -> Result<Self, String> {
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(8);
        let open = |file: &str| -> Result<Session, String> {
            Session::builder()
                .map_err(ort_error)?
                .with_intra_threads(threads)
                .map_err(ort_error)?
                .commit_from_file(dir.join(file))
                .map_err(ort_error)
        };
        let config: Config = serde_json::from_str(
            &std::fs::read_to_string(dir.join("nemotron.json")).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            frontend: open("frontend.onnx")?,
            step: open(step_file)?,
            config,
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
        let embeds = self.embeds(samples)?;
        let logits = self.chunks(&embeds, events, abort)?;
        let frames = 1 + samples.len() / HOP;
        let n = self.config.num_speakers;
        Ok(logits[..(frames * n).min(logits.len())]
            .iter()
            .map(|l| 1.0 / (1.0 + (-l).exp()))
            .collect())
    }

    /// The encoder input for the whole file: `steps x hidden_size`.
    fn embeds(&mut self, samples: &[f32]) -> Result<Vec<f32>, String> {
        let spectrum = Spectrum::new(samples);
        let frames = spectrum.frames;
        let valid = samples.len() / HOP;
        let mut embeds = Vec::new();
        let mut start = 0;
        while start < frames {
            let end = (start + FRONTEND_BLOCK).min(frames);
            let rows = (end - start).div_ceil(8) * 8;
            let mut block = spectrum.power(start, end);
            block.resize(rows * BINS, 0.0);
            let valid_here = valid.saturating_sub(start).min(rows) as i64;
            let outputs = self
                .frontend
                .run(ort::inputs![
                    "power" => Tensor::from_array(([1usize, rows, BINS], block)).map_err(ort_error)?,
                    "valid" => Tensor::from_array(([1usize], vec![valid_here])).map_err(ort_error)?,
                ])
                .map_err(ort_error)?;
            let (_, data) = outputs["embeds"]
                .try_extract_tensor::<f32>()
                .map_err(ort_error)?;
            embeds.extend_from_slice(data);
            start = end;
        }
        Ok(embeds)
    }

    /// The offline forward: chunks of `chunk_length` steps with look-ahead,
    /// each run together with the speaker cache and the FIFO.
    fn chunks(
        &mut self,
        embeds: &[f32],
        events: &Events,
        abort: &Abort,
    ) -> Result<Vec<f32>, String> {
        let h = self.config.hidden_size;
        let factor = self.config.subsampling_factor;
        let n = self.config.num_speakers;
        let steps = embeds.len() / h;
        let mut cache = Cache::new(&self.config);
        let mut logits = Vec::with_capacity(steps * factor * n);
        let mut start = 0;
        while start < steps {
            if abort.load(Ordering::Relaxed) {
                return Err(CANCELLED.into());
            }
            let end = (start + self.config.chunk_length).min(steps);
            let chunk_frames = end - start;
            let with_lookahead = (end + self.config.chunk_right_context).min(steps);
            let cached = cache.embeds();
            let cached_len = cached.len() / h;
            let mut input = cached;
            input.extend_from_slice(&embeds[start * h..with_lookahead * h]);
            let rows = input.len() / h;
            let outputs = self
                .step
                .run(ort::inputs![
                    "embeds" => Tensor::from_array(([1usize, rows, h], input.clone())).map_err(ort_error)?,
                ])
                .map_err(ort_error)?;
            let (_, step_logits) = outputs["logits"]
                .try_extract_tensor::<f32>()
                .map_err(ort_error)?;
            logits.extend_from_slice(
                &step_logits[cached_len * factor * n..(cached_len + chunk_frames) * factor * n],
            );
            cache.update(&input, step_logits, chunk_frames);
            let _ = events.send_blocking(Event::Progress(end as f64 / steps as f64));
            start = end;
        }
        Ok(logits)
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
struct Cache<'a> {
    config: &'a Config,
    cache_embeds: Vec<f32>,
    cache_probs: Vec<f32>,
    fifo: Vec<f32>,
    compressed: bool,
}

impl<'a> Cache<'a> {
    fn new(config: &'a Config) -> Self {
        Self {
            config,
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
    fn update(&mut self, input: &[f32], logits: &[f32], chunk_frames: usize) {
        let c = self.config;
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
                (cache_embeds, cache_probs) = self.compress(&cache_embeds, &cache_probs);
                self.compressed = true;
            }
            self.cache_embeds = cache_embeds;
            self.cache_probs = cache_probs;
        }
        self.fifo = fifo;
    }

    /// Frame scores for the cache: high for frames that clearly belong to one
    /// speaker; -inf for frames that are not that speaker's speech.
    fn frame_scores(&self, probs: &[f32], frames: usize) -> Vec<f32> {
        let c = self.config;
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
    fn compress(&self, embeds: &[f32], probs: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let c = self.config;
        let (h, n) = (c.hidden_size, c.num_speakers);
        let frames = embeds.len() / h;
        let mut scores = self.frame_scores(probs, frames);
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
