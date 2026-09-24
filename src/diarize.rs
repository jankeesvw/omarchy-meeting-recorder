//! Who speaks when, in a single audio file.
//!
//! A recording made by the app has two tracks, so the speaker of every line
//! follows from which track is louder. An imported file has one, so the voices
//! themselves have to be told apart. That is NVIDIA's Nemotron 3 Diarization
//! (see `nemotron.rs`), run locally: it follows up to eight speakers, also when
//! they talk at the same time.

use std::path::PathBuf;

use crate::transcribe::{Abort, Event, Events, WHISPER_RATE, models_dir};

/// Where the exported model lives: `frontend.onnx`, the step graph and
/// `nemotron.json`.
fn model_dir() -> PathBuf {
    models_dir().join("nemotron-3-diarization")
}

/// The step graph: int8 by default (101 MB), `NEMOTRON_STEP=step.onnx` for
/// the full-precision export (396 MB).
fn step_file() -> String {
    std::env::var("NEMOTRON_STEP").unwrap_or_else(|_| "step-int8.onnx".into())
}

/// A stretch of one speaker. `speaker` counts from 0 in the order the voices
/// are first heard.
#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub start_ms: i64,
    pub end_ms: i64,
    pub speaker: usize,
}

/// Finds the speakers in `samples` (16 kHz mono). `speakers` fixes how many
/// there are; `None` lets the model decide.
pub fn turns(
    samples: &[f32],
    speakers: Option<usize>,
    events: &Events,
    abort: &Abort,
) -> Result<Vec<Turn>, String> {
    let dir = model_dir();
    let _ = events.send_blocking(Event::Stage("Finding speakers".into()));
    let _ = events.send_blocking(Event::Progress(0.0));
    let mut model = crate::nemotron::Model::load(&dir, &step_file())
        .map_err(|e| format!("{e} (the speaker model is expected in {})", dir.display()))?;
    let probs = model.probabilities(samples, events, abort)?;
    let raw = segments(&probs, 8);
    let raw = match speakers {
        Some(n) => keep_largest(raw, n),
        // Voices heard for only a few seconds are almost always one of the
        // others on a bad moment (a cough, a laugh, crosstalk).
        None => absorb_small_clusters(raw),
    };
    Ok(renumber(raw))
}

/// Stretches where a speaker's probability is over one half, in ms. A frame
/// is 10 ms; pauses under half a second within one speaker are bridged and
/// blips under 0.3 seconds dropped, as the old diarization did.
fn segments(probs: &[f32], speakers: usize) -> Vec<(i64, i64, i32)> {
    let frames = probs.len() / speakers;
    let mut raw = Vec::new();
    for s in 0..speakers {
        let mut runs: Vec<(i64, i64)> = Vec::new();
        let mut start = None;
        for f in 0..=frames {
            let on = f < frames && probs[f * speakers + s] > 0.5;
            match (on, start) {
                (true, None) => start = Some(f),
                (false, Some(from)) => {
                    let (from, to) = (from as i64 * 10, f as i64 * 10);
                    match runs.last_mut() {
                        Some(last) if from - last.1 < 500 => last.1 = to,
                        _ => runs.push((from, to)),
                    }
                    start = None;
                }
                _ => {}
            }
        }
        raw.extend(
            runs.into_iter()
                .filter(|(from, to)| to - from >= 300)
                .map(|(from, to)| (from, to, s as i32)),
        );
    }
    raw
}

/// Keeps the `n` speakers with the most speech; the turns of the others go to
/// the nearest kept speaker.
fn keep_largest(raw: Vec<(i64, i64, i32)>, n: usize) -> Vec<(i64, i64, i32)> {
    let mut spoken = std::collections::HashMap::<i32, i64>::new();
    for (start, end, id) in &raw {
        *spoken.entry(*id).or_default() += end - start;
    }
    let mut ranked: Vec<(i32, i64)> = spoken.into_iter().collect();
    ranked.sort_by_key(|(id, ms)| (-ms, *id));
    let kept: Vec<i32> = ranked.iter().take(n.max(1)).map(|(id, _)| *id).collect();
    reassign(raw, |id| kept.contains(id))
}

/// Gives every cluster with little speech (under 4 seconds, or under 4% of
/// all speech) to the speaker of the nearest turn from a cluster that stays.
fn absorb_small_clusters(raw: Vec<(i64, i64, i32)>) -> Vec<(i64, i64, i32)> {
    let mut spoken = std::collections::HashMap::<i32, i64>::new();
    for (start, end, id) in &raw {
        *spoken.entry(*id).or_default() += end - start;
    }
    let total: i64 = spoken.values().sum();
    let floor = (total * 4 / 100).max(4000);
    reassign(raw, |id| spoken.get(id).is_some_and(|ms| *ms >= floor))
}

/// Moves the turns of every speaker that `keeps` rejects to the speaker of
/// the nearest kept turn.
fn reassign(raw: Vec<(i64, i64, i32)>, keeps: impl Fn(&i32) -> bool) -> Vec<(i64, i64, i32)> {
    let anchors: Vec<(i64, i64, i32)> =
        raw.iter().copied().filter(|(_, _, id)| keeps(id)).collect();
    if anchors.is_empty() {
        return raw;
    }
    raw.iter()
        .map(|&(start, end, id)| {
            if keeps(&id) {
                return (start, end, id);
            }
            let middle = (start + end) / 2;
            let nearest = anchors
                .iter()
                .min_by_key(|(s, e, _)| {
                    if middle < *s {
                        s - middle
                    } else {
                        (middle - e).max(0)
                    }
                })
                .map_or(id, |(_, _, other)| *other);
            (start, end, nearest)
        })
        .collect()
}

/// Sorts the turns and numbers the speakers in the order they are first heard.
fn renumber(mut raw: Vec<(i64, i64, i32)>) -> Vec<Turn> {
    raw.sort_by_key(|(start, _, _)| *start);
    let mut order: Vec<i32> = Vec::new();
    raw.into_iter()
        .map(|(start_ms, end_ms, id)| {
            let speaker = order.iter().position(|o| *o == id).unwrap_or_else(|| {
                order.push(id);
                order.len() - 1
            });
            Turn {
                start_ms,
                end_ms,
                speaker,
            }
        })
        .collect()
}

/// The whole file as one speaker, for when there is only one.
pub fn single(samples: &[f32]) -> Vec<Turn> {
    vec![Turn {
        start_ms: 0,
        end_ms: (samples.len() * 1000 / WHISPER_RATE) as i64,
        speaker: 0,
    }]
}

/// The speaker of `start_ms..end_ms`: the one whose turns overlap it most, or
/// the nearest turn when none do (whisper's words can fall in a gap).
pub fn speaker_at(turns: &[Turn], start_ms: i64, end_ms: i64) -> usize {
    let end_ms = end_ms.max(start_ms + 1);
    let mut overlap = std::collections::HashMap::<usize, i64>::new();
    for turn in turns {
        let shared = turn.end_ms.min(end_ms) - turn.start_ms.max(start_ms);
        if shared > 0 {
            *overlap.entry(turn.speaker).or_default() += shared;
        }
    }
    if let Some((speaker, _)) = overlap
        .into_iter()
        .max_by_key(|(s, o)| (*o, usize::MAX - s))
    {
        return speaker;
    }
    let middle = (start_ms + end_ms) / 2;
    turns
        .iter()
        .min_by_key(|t| {
            if middle < t.start_ms {
                t.start_ms - middle
            } else {
                (middle - t.end_ms).max(0)
            }
        })
        .map_or(0, |t| t.speaker)
}

/// Where `speaker`'s turn starts near `around_ms`, within a second and a half.
pub fn turn_start_near(turns: &[Turn], speaker: usize, around_ms: i64) -> Option<i64> {
    turns
        .iter()
        .filter(|t| t.speaker == speaker && (t.start_ms - around_ms).abs() <= 1500)
        .min_by_key(|t| (t.start_ms - around_ms).abs())
        .map(|t| t.start_ms)
}

/// `diarize <audio> [--speakers N]`: prints the speaker turns as JSON, for
/// comparing diarization engines on the same file.
pub fn cli(args: &[String]) -> gtk::glib::ExitCode {
    let mut path = None;
    let mut speakers = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--speakers" | "-s" => speakers = iter.next().and_then(|n| n.parse::<usize>().ok()),
            other => path = Some(PathBuf::from(other)),
        }
    }
    let Some(path) = path else {
        eprintln!("Usage: {} diarize <audio> [--speakers N]", crate::APP_NAME);
        return gtk::glib::ExitCode::from(2);
    };
    let result = crate::transcribe::load_track(&path).and_then(|samples| {
        let (events, _rx) = async_channel::unbounded();
        let started = std::time::Instant::now();
        let turns = turns(&samples, speakers, &events, &Abort::default())?;
        eprintln!(
            "{} turns in {:.1}s for {}s of audio",
            turns.len(),
            started.elapsed().as_secs_f64(),
            samples.len() / WHISPER_RATE
        );
        Ok(turns)
    });
    match result {
        Ok(turns) => {
            let json: Vec<serde_json::Value> = turns
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "speaker": t.speaker,
                        "start": t.start_ms as f64 / 1000.0,
                        "end": t.end_ms as f64 / 1000.0,
                    })
                })
                .collect();
            println!("{}", serde_json::Value::Array(json));
            gtk::glib::ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{}: {message}", crate::APP_NAME);
            gtk::glib::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speakers_are_numbered_by_first_appearance() {
        let turns = renumber(vec![
            (5000, 6000, 3),
            (0, 1000, 7),
            (2000, 3000, 3),
            (7000, 8000, 7),
        ]);
        let order: Vec<usize> = turns.iter().map(|t| t.speaker).collect();
        assert_eq!(order, vec![0, 1, 1, 0]);
    }

    #[test]
    fn words_go_to_the_turn_they_overlap_most() {
        let turns = vec![
            Turn {
                start_ms: 0,
                end_ms: 2000,
                speaker: 0,
            },
            Turn {
                start_ms: 1800,
                end_ms: 5000,
                speaker: 1,
            },
        ];
        assert_eq!(speaker_at(&turns, 1500, 1900), 0);
        assert_eq!(speaker_at(&turns, 1900, 3000), 1);
        // In a gap after the last turn: the nearest one.
        assert_eq!(speaker_at(&turns, 6000, 6500), 1);
        assert_eq!(turn_start_near(&turns, 1, 2500), Some(1800));
        assert_eq!(turn_start_near(&turns, 1, 9000), None);
    }
}
