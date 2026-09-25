#!/usr/bin/env python3
"""Runs the app's command line over the test cases and scores the results.

    bench/run.py [--bin PATH] [--case NAME ...] [--ami] [--json FILE]

--bin     the binary to test (default: target/release/omarchy-meeting-recorder)
--case    only these cases (default: all)
--ami     also a real meeting from the AMI corpus, downloaded to bench/.cache
--ami-minutes  how much of that meeting (default 5, 0 for all 17 minutes)
--json    write the scores to FILE, to compare two binaries or models later
--check   fail (exit 1) when a case scores below bench/thresholds.json
--model   the whisper model to use (default: the app's own setting)
--keep    save every transcript and speaker turns in DIR, to look into a score

Columns: found = words of the script that are in the transcript; side = of
those, on the right side of the call (you or the other side); person = with
the right person; leaked = lines of yours that are really the other side
leaking into your mic; speakers = voices told apart / voices in the case;
speaker error = share of speech given to the wrong speaker by `diarize`.
For AMI there is no script, so side and person come from who spoke when.
Only Python's standard library is needed, plus ffmpeg for AMI.
"""
import argparse
import json
import re
import subprocess
import sys
import time
import urllib.request
from collections import Counter, defaultdict
from itertools import permutations
from pathlib import Path

HERE = Path(__file__).resolve().parent
LINE = re.compile(r"\*\*\[(?:(\d+):)?(\d+):(\d+)\] ([^:*]+):\*\* (.*)")
AMI = "https://groups.inf.ed.ac.uk/ami/AMICorpusMirror/amicorpus"
RTTM = "https://raw.githubusercontent.com/pyannote/AMI-diarization-setup/main/only_words/rttms/test"


def words(text):
    return re.findall(r"[a-z0-9']+", text.lower())


def side_of(label):
    for side, where in (("You", "mic"), ("Remote", "computer")):
        if label == side or re.fullmatch(side + r" \d+", label):
            return where
    return None


def parse(markdown):
    lines = []
    for m in LINE.finditer(markdown):
        start = int(m[1] or 0) * 3600 + int(m[2]) * 60 + int(m[3])
        lines.append({"start": start, "label": m[4].strip(), "words": words(m[5])})
    for i, line in enumerate(lines):
        later = [l["start"] for l in lines[i + 1:] if l["start"] > line["start"]]
        line["end"] = min(later[0] if later else line["start"] + 30, line["start"] + 60)
    return lines


def best_mapping(votes, names):
    """Label -> truth speaker, one to one, by the most shared words or seconds."""
    labels = sorted(votes)
    best, score = {}, -1
    if len(labels) <= len(names):
        for perm in permutations(names, len(labels)):
            s = sum(votes[l][p] for l, p in zip(labels, perm))
            if s > score:
                score, best = s, dict(zip(labels, perm))
    else:
        for perm in permutations(labels, len(names)):
            s = sum(votes[l][n] for l, n in zip(perm, names))
            if s > score:
                score, best = s, dict(zip(perm, names))
    return best


def overlaps(line, truth, margin=1.5):
    return line["start"] - margin <= truth["end"] and truth["start"] <= line["end"] + margin


def score_text(markdown, truth):
    """Against a script: words found, on the right side, with the right person."""
    lines = parse(markdown)
    if not truth:
        return {"lines": len(lines)}
    names = sorted({t["speaker"] for t in truth})
    votes = defaultdict(Counter)
    for t in truth:
        tw = Counter(words(t["text"]))
        for l in lines:
            if overlaps(l, t):
                votes[l["label"]][t["speaker"]] += sum((tw & Counter(l["words"])).values())
    person = best_mapping(votes, names)
    total = found = side = right = 0
    for t in truth:
        tw = Counter(words(t["text"]))
        n = sum(tw.values())
        total += n
        near = [l for l in lines if overlaps(l, t)]
        pool = lambda keep: sum((tw & Counter(w for l in near if keep(l) for w in l["words"])).values())
        found += min(n, pool(lambda l: True))
        side += min(n, pool(lambda l: side_of(l["label"]) == t["side"]))
        right += min(n, pool(lambda l: person.get(l["label"]) == t["speaker"]))
    # A line of yours that is mostly words the other side said at that moment.
    leaked = 0
    for l in lines:
        if side_of(l["label"]) != "mic" or not l["words"]:
            continue
        theirs = Counter(w for t in truth if t["side"] == "computer" and overlaps(l, t) for w in words(t["text"]))
        mine = Counter(w for t in truth if t["side"] == "mic" and overlaps(l, t) for w in words(t["text"]))
        own = Counter(l["words"])
        if sum((own & theirs).values()) > max(0.6 * sum(own.values()), sum((own & mine).values())):
            leaked += 1
    return {"found": found / total, "side": side / total, "person": right / total, "leaked": leaked,
            "speakers": f"{len(votes)}/{len(names)}"}


def score_timing(markdown, truth):
    """Without a script: whether each line went to whoever spoke then, weighted by words."""
    lines = parse(markdown)
    names = sorted({t["speaker"] for t in truth})

    def spoken(a, b):
        c = Counter()
        for t in truth:
            o = min(b, t["end"]) - max(a, t["start"])
            if o > 0:
                c[t["speaker"]] += o
        return c

    votes = defaultdict(Counter)
    for l in lines:
        for who, secs in spoken(l["start"], l["end"]).items():
            votes[l["label"]][who] += secs
    person = best_mapping(votes, names)
    sides = {t["speaker"]: t["side"] for t in truth}
    total = side = right = 0
    for l in lines:
        c = spoken(l["start"], l["end"])
        if not c:
            continue
        who = c.most_common(1)[0][0]
        n = len(l["words"])
        total += n
        side += n if side_of(l["label"]) == sides.get(who) else 0
        right += n if person.get(l["label"]) == who else 0
    return {"side": side / max(total, 1), "person": right / max(total, 1), "speakers": f"{len(votes)}/{len(names)}"}


def score_turns(turns, truth, step=0.1):
    """`diarize` against who spoke when: speech given to the wrong speaker."""
    end = max(t["end"] for t in truth)
    n = int(end / step) + 1
    label = [None] * n
    for t in truth:
        for i in range(int(t["start"] / step), min(n, int(t["end"] / step))):
            label[i] = t["speaker"] if label[i] in (None, t["speaker"]) else "#"
    hyp = [set() for _ in range(n)]
    for t in turns:
        for i in range(int(t["start"] / step), min(n, int(t["end"] / step))):
            hyp[i].add(t["speaker"])
    pairs, scored, missed = Counter(), 0, 0
    for t, h in zip(label, hyp):
        if t in (None, "#"):
            continue
        scored += 1
        if not h:
            missed += 1
        for s in h:
            pairs[(t, s)] += 1 / len(h)
    votes = defaultdict(Counter)
    for (t, s), v in pairs.items():
        votes[s][t] += v
    mapping = best_mapping(votes, sorted({t for t in label if t not in (None, "#")}))
    matched = sum(votes[s][t] for s, t in mapping.items())
    return {"speaker error": (scored - missed - matched) / max(scored, 1),
            "speakers": f"{len({t['speaker'] for t in turns})}/{len({t['speaker'] for t in truth})}"}


def run(binary, args):
    started = time.time()
    out = subprocess.run([binary, *args], capture_output=True, text=True)
    if out.returncode != 0:
        raise RuntimeError(out.stderr.strip().splitlines()[-1] if out.stderr.strip() else "failed")
    return out.stdout, time.time() - started


def fetch(url, target):
    if not target.exists():
        print(f"  downloading {url.rsplit('/', 1)[-1]}", file=sys.stderr)
        target.parent.mkdir(parents=True, exist_ok=True)
        urllib.request.urlretrieve(url, target.with_suffix(".part"))
        target.with_suffix(".part").rename(target)
    return target


def cut(source, minutes, target):
    """The first `minutes` of `source` (all of it when minutes is 0)."""
    if not minutes:
        return source
    if not target.exists():
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", str(source), "-t", str(minutes * 60), str(target)],
                       check=True)
    return target


def ami_cases(minutes, meeting="ES2004a"):
    """A real four-person meeting, as an imported file and as a call: the
    first person's headset is your mic, the other three are the computer audio.
    `minutes` keeps only the start of it, 0 the whole meeting."""
    cache = HERE / ".cache" / meeting
    rttm = fetch(f"{RTTM}/{meeting}.rttm", cache / f"{meeting}.rttm")
    mix = fetch(f"{AMI}/{meeting}/audio/{meeting}.Mix-Headset.wav", cache / "mix.wav")
    heads = [fetch(f"{AMI}/{meeting}/audio/{meeting}.Headset-{i}.wav", cache / f"headset-{i}.wav") for i in range(4)]
    truth = []
    for row in open(rttm):
        f = row.split()
        truth.append({"speaker": f[7], "start": float(f[3]), "end": float(f[3]) + float(f[4])})
    # Which headset belongs to whom: the speaker whose turns are loudest on it.
    loud = []
    for h in heads:
        raw = subprocess.run(["ffmpeg", "-v", "error", "-i", str(h), "-ac", "1", "-ar", "100", "-f", "s16le", "-"],
                             capture_output=True, check=True).stdout
        level = [abs(int.from_bytes(raw[i:i + 2], "little", signed=True)) for i in range(0, len(raw), 2)]
        per = Counter()
        for t in truth:
            span = level[int(t["start"] * 100):int(t["end"] * 100)]
            per[t["speaker"]] += sum(span) / max(len(span), 1)
        loud.append(per)
    mine = loud[0].most_common(1)[0][0]
    for t in truth:
        t["side"] = "mic" if t["speaker"] == mine else "computer"
    computer = cache / "computer.wav"
    if not computer.exists():
        subprocess.run(["ffmpeg", "-v", "error", "-y", *[a for h in heads[1:] for a in ("-i", str(h))],
                        "-filter_complex", "amix=inputs=3:normalize=0", str(computer)], check=True)
    if minutes:
        end = minutes * 60
        truth = [dict(t, end=min(t["end"], end)) for t in truth if t["start"] < end]
    part = f"-{minutes}m" if minutes else ""
    about = f"AMI {meeting}: a real four-person meeting (CC BY 4.0)" + (f", first {minutes} minutes" if minutes else "")
    return [
        {"name": f"ami-{meeting}-import", "kind": "import", "about": about, "truth": truth,
         "audio": cut(mix, minutes, cache / f"mix{part}.wav")},
        {"name": f"ami-{meeting}-call", "kind": "call", "about": about, "truth": truth,
         "mic": cut(heads[0], minutes, cache / f"headset-0{part}.wav"),
         "computer": cut(computer, minutes, cache / f"computer{part}.wav")},
    ]


def fixtures():
    for directory in sorted((HERE / "fixtures").iterdir()):
        case = json.load(open(directory / "truth.json"))
        case["name"] = directory.name
        for track in ("mic", "computer", "audio"):
            if (directory / f"{track}.ogg").exists():
                case[track] = directory / f"{track}.ogg"
        yield case


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--bin", default=str(HERE.parent / "target/release/omarchy-meeting-recorder"))
    parser.add_argument("--case", nargs="*")
    parser.add_argument("--ami", action="store_true")
    parser.add_argument("--ami-minutes", type=int, default=5)
    parser.add_argument("--json")
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--model")
    parser.add_argument("--keep")
    args = parser.parse_args()

    cases = list(fixtures()) + (ami_cases(args.ami_minutes) if args.ami else [])
    if args.case:
        cases = [c for c in cases if c["name"] in args.case]
    model = ["--model", args.model] if args.model else []
    if args.keep:
        Path(args.keep).mkdir(parents=True, exist_ok=True)
    results = {}
    for case in cases:
        name = case["name"]
        print(f"{name}: {case['about']}", file=sys.stderr)
        scored = {"about": case["about"]}
        text = bool(case["truth"]) and "text" in case["truth"][0]
        try:
            if case["kind"] == "call":
                md, secs = run(args.bin, ["transcribe", str(case["mic"]), str(case["computer"]), "--language", "en", *model])
            else:
                md, secs = run(args.bin, ["transcribe-file", str(case["audio"]), "--language", "en", *model])
                turns, _ = run(args.bin, ["diarize", str(case["audio"])])
                scored.update(score_turns(json.loads(turns), case["truth"]))
                if args.keep:
                    Path(args.keep, f"{name}.turns.json").write_text(turns)
            scored["seconds"] = round(secs, 1)
            if args.keep:
                Path(args.keep, f"{name}.md").write_text(md)
            scored.update(score_text(md, case["truth"]) if text or not case["truth"] else score_timing(md, case["truth"]))
            if case["kind"] == "import":
                scored.pop("side", None)
        except Exception as e:  # a failing case is a result too
            scored["error"] = str(e)
        results[name] = scored

    columns = ["found", "side", "person", "leaked", "lines", "speakers", "speaker error", "seconds"]
    shown = [c for c in columns if any(c in r for r in results.values())]
    width = max(len(n) for n in results) + 2
    print("case".ljust(width) + "".join(c.rjust(15) for c in shown))
    for name, r in results.items():
        cells = []
        for c in shown:
            v = r.get(c, "")
            cells.append((f"{v:.1%}" if isinstance(v, float) and c not in ("seconds",) else str(v)).rjust(15))
        print(name.ljust(width) + "".join(cells) + (f"   error: {r['error']}" if "error" in r else ""))
    failures = check(results, json.load(open(HERE / "thresholds.json"))) if args.check else []
    if args.json:
        json.dump({"cases": results, "failures": failures}, open(args.json, "w"), indent=1)
    if args.check:
        for failure in failures:
            print(f"FAIL {failure}")
        if failures:
            sys.exit(1)
        print("All cases meet their thresholds.")


def check(results, thresholds):
    """`min` scores must be reached, `max` counts (leaked lines, lines in
    silence, speaker error) not exceeded, and every voice found."""
    failures = []
    for name, limits in thresholds.items():
        r = results.get(name)
        if r is None:
            continue
        if "error" in r:
            failures.append(f"{name}: {r['error']}")
            continue
        for key, least in limits.get("min", {}).items():
            if r.get(key, 0) < least:
                failures.append(f"{name}: {key} {r.get(key, 0):.1%} is below {least:.1%}")
        for key, most in limits.get("max", {}).items():
            if r.get(key, 0) > most:
                failures.append(f"{name}: {key} {r.get(key, 0)} is above {most}")
        if limits.get("all_speakers") and "speakers" in r:
            found, wanted = r["speakers"].split("/")
            if found != wanted:
                failures.append(f"{name}: found {found} of {wanted} speakers")
    return failures


if __name__ == "__main__":
    main()
