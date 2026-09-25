#!/usr/bin/env python3
"""Generates the synthetic test cases in bench/fixtures from bench/scripts.

    bench/generate.py <piper-voices-dir>

Needs piper-tts and ffmpeg, and these voices from
https://huggingface.co/rhasspy/piper-voices (all public domain or CC0):
en_US-joe-medium, en_US-john-medium, en_US-kristin-medium,
en_US-norman-medium, en_GB-cori-medium.

A script line is `speaker|gap|text`, gap being seconds after the previous
line ends (negative: they start before the other is done). The fixtures are
committed, so running the bench does not need any of this.
"""
import array
import json
import math
import random
import subprocess
import sys
import tempfile
import wave
from pathlib import Path

RATE = 22050
HERE = Path(__file__).resolve().parent
VOICES = {
    "You": "en_US-joe-medium",
    "Dave": "en_US-john-medium",
    "Anna": "en_US-kristin-medium",
    "Ben": "en_US-norman-medium",
    "Carla": "en_GB-cori-medium",
    "Kristin": "en_US-kristin-medium",
    "Norman": "en_US-norman-medium",
    "Cori": "en_GB-cori-medium",
}
# People in the room with you are on the mic; everyone else on the computer audio.
LOCAL = {"You", "Dave"}
# Not everyone is equally loud on a call.
GAIN = {"You": 0.8, "Dave": 0.55, "Anna": 0.9, "Ben": 0.7, "Carla": 1.0,
        "Kristin": 0.9, "Norman": 0.75, "Cori": 0.85}


def synth(voices: Path, voice: str, text: str, tmp: Path) -> list[float]:
    out = tmp / "line.wav"
    subprocess.run(["piper-tts", "--model", str(voices / f"{voice}.onnx"), "--output_file", str(out)],
                   input=text.encode(), check=True, capture_output=True)
    with wave.open(str(out)) as w:
        assert w.getframerate() == RATE, voice
        a = array.array("h")
        a.frombytes(w.readframes(w.getnframes()))
    return [x / 32768 for x in a]


def render(voices: Path, script: str, tmp: Path):
    lines = [l.rstrip("\n").split("|", 2) for l in open(HERE / "scripts" / script) if l.strip()]
    clips, prev_end = [], 0.0
    for who, gap, text in lines:
        samples = synth(voices, VOICES[who], text, tmp)
        start = max(0.0, prev_end + float(gap))
        clips.append((who, start, samples, text))
        prev_end = max(prev_end, start + len(samples) / RATE)
    n = int((prev_end + 1.0) * RATE)
    mic, computer, truth = [0.0] * n, [0.0] * n, []
    for who, start, samples, text in clips:
        at = int(start * RATE)
        track = mic if who in LOCAL else computer
        for i, s in enumerate(samples):
            track[at + i] += s * GAIN[who]
        truth.append({"speaker": who, "side": "mic" if who in LOCAL else "computer",
                      "start": round(start, 2), "end": round(start + len(samples) / RATE, 2),
                      "text": text})
    return mic, computer, truth


def noise(n: int, level: float, seed: int) -> list[float]:
    rnd = random.Random(seed)
    return [rnd.gauss(0, level) for _ in range(n)]


def music(n: int) -> list[float]:
    """A steady bed of chords with a soft pulse, like a radio in the background."""
    chords = [(220.0, 277.2, 329.6), (196.0, 246.9, 293.7), (174.6, 220.0, 261.6), (196.0, 246.9, 293.7)]
    out = []
    for i in range(n):
        t = i / RATE
        chord = chords[int(t / 2) % len(chords)]
        pulse = 0.6 + 0.4 * math.exp(-((t * 2) % 1) * 6)
        out.append(0.05 * pulse * sum(math.sin(2 * math.pi * f * t) for f in chord))
    return out


def leak(mic: list[float], computer: list[float], amount: float) -> list[float]:
    """The other side through your speakers into your mic, 40 ms late."""
    delay = int(0.04 * RATE)
    return [m + amount * (computer[i - delay] if i >= delay else 0.0) for i, m in enumerate(mic)]


def mix(*tracks: list[float]) -> list[float]:
    return [sum(v) for v in zip(*tracks)]


def write(case: str, name: str, data: list[float], tmp: Path):
    directory = HERE / "fixtures" / case
    directory.mkdir(parents=True, exist_ok=True)
    raw = tmp / f"{name}.wav"
    with wave.open(str(raw), "w") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(array.array("h", [int(max(-1.0, min(1.0, x)) * 32767) for x in data]).tobytes())
    # Opus, like the app's own recordings: what the app reads in practice, and small.
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", str(raw), "-ac", "1", "-c:a", "libopus",
                    "-b:a", "48k", str(directory / f"{name}.ogg")], check=True)


def case(name: str, kind: str, about: str, truth: list, tmp: Path, **tracks):
    for track, data in tracks.items():
        write(name, track, data, tmp)
    directory = HERE / "fixtures" / name
    json.dump({"kind": kind, "about": about, "truth": truth}, open(directory / "truth.json", "w"), indent=1)
    print(f"{name}: {len(truth)} lines, {len(next(iter(tracks.values()))) / RATE:.0f} s")


def main():
    voices = Path(sys.argv[1])
    with tempfile.TemporaryDirectory() as t:
        tmp = Path(t)
        mic, computer, truth = render(voices, "call.txt", tmp)
        hiss = noise(len(mic), 0.002, 1)
        case("call", "call", "You on a headset, three people on the other side who interrupt each other.",
             truth, tmp, mic=mix(mic, hiss), computer=computer)
        case("call-speakers", "call", "The same call through speakers: the other side leaks into your mic.",
             truth, tmp, mic=leak(mix(mic, hiss), computer, 0.18), computer=computer)
        case("import", "import", "The same call as one mixed file, four voices.",
             truth, tmp, audio=mix(mic, computer))

        mic, computer, truth = render(voices, "room.txt", tmp)
        case("room", "call", "Two people share your mic, two people on the other side.",
             truth, tmp, mic=mix(mic, noise(len(mic), 0.002, 2)), computer=computer)

        mic, computer, truth = render(voices, "music.txt", tmp)
        case("music", "call", "The other side talks with music playing in the background.",
             truth, tmp, mic=mix(mic, noise(len(mic), 0.002, 3)), computer=mix(computer, music(len(computer))))

        n = 20 * RATE
        case("silence", "call", "Twenty seconds of room noise and nothing said: the transcript must be empty.",
             [], tmp, mic=noise(n, 0.003, 4), computer=noise(n, 0.001, 5))


if __name__ == "__main__":
    main()
