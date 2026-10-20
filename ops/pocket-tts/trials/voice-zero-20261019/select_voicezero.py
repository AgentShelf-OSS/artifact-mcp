"""Filter, de-duplicate, balance, and name the Voice-Zero narrator candidates.

Usage: select_voicezero.py <vz-work-dir> <catalog.json> <out.json>
                           --baseline DIR [--baseline DIR ...] [--reserved-labels file.json ...]
                           [--max-keep 50] [--accent-cap 0.4]

Inputs (vz-work-dir/out): metrics-raw.json, embeddings.npz, f0.json, export-manifest.json,
synth-*.jsonl. Each --baseline DIR holds embeddings.npz (and f0.json) from an earlier run with
the same model, seed, and sentences: the donation trial (built-ins, extras, prepared
Voice-Zero, Kyutai candidates) and the VCTK trial. Every voice in catalog.json (the live
catalog) must have a baseline embedding. --reserved-labels: JSON lists of labels (or a
voice-metrics.json, whose candidate labels are used) that a new name must not repeat.

Thresholds are the donation audition's (see ../kyutai-donations-20261018/README.md).
Accent and region are Voice-Zero's README notes (the author calls them rough guesses).
Gender is the donation trial's estimate: median F0 < 145 Hz male, >= 175 Hz female, and
null when the 7 nearest F0-confident speaker-embedding neighbours disagree.
Nothing here is a listening judgement; Neil decides in the audition.
"""
import argparse, json
from collections import Counter
from pathlib import Path
import numpy as np

ap = argparse.ArgumentParser()
ap.add_argument("work"); ap.add_argument("catalog"); ap.add_argument("out")
ap.add_argument("--baseline", action="append", required=True)
ap.add_argument("--reserved-labels", action="append", default=[])
ap.add_argument("--max-keep", type=int, default=50)
ap.add_argument("--accent-cap", type=float, default=0.4)
args = ap.parse_args()
W = Path(args.work) / "out"

WER_DROP, WER_ONE_DROP = 0.12, 0.25
MAX_GAP, MAX_EDGE, WPM = 2.0, 1.5, (120, 250)
CLIP_MAX = 0.001
DUP = 0.975
TARGET = -18.63          # loudness target from the donation audition (median LUFS + gain of exposed voices)
SENTENCES = {"neutral": "Please check the schedule before Thursday. The meeting has moved to room twelve, near the north entrance.",
             "story": "Long ago, in a village by the sea, an old woman kept a lantern burning every night, waiting for a ship that never came home."}

# Narrator names as the LibriVox reader pages give them. The Voice-Zero README misspells
# one; labels must be printable ASCII, so the accented name is kept for the credit only.
NARRATOR_FIX = {"Caden Vaugn Clegg": "Caden Vaughn Clegg"}
NARRATOR_CREDIT = {"Delibab": "Délibáb"}

metrics = json.loads((W / "metrics-raw.json").read_text())
emb = dict(np.load(W / "embeddings.npz"))
base_emb, base_f0 = {}, {}
for d in args.baseline:
    for k, e in np.load(Path(d) / "embeddings.npz").items():
        base_emb.setdefault(k, e)
    if (Path(d) / "f0.json").exists():
        for k, f in json.loads((Path(d) / "f0.json").read_text()).items():
            base_f0.setdefault(k, f["f0_median"])
F0 = json.loads((W / "f0.json").read_text())
export = {r["voice"]: r for r in json.loads((W / "export-manifest.json").read_text())["results"]}
catalog = json.loads(Path(args.catalog).read_text())["voices"]
exposed = [v["id"] for v in catalog]
missing = [v for v in exposed if v not in base_emb]
if missing:
    raise SystemExit(f"no baseline embedding for exposed voices: {missing}")
clash = set(export) & set(base_emb)
if clash:
    raise SystemExit(f"candidate ids clash with baseline voices: {sorted(clash)}")
for v, f in F0.items():
    if v in metrics:
        metrics[v]["f0_median"], metrics[v]["f0_iqr"] = f["f0_median"], f["f0_iqr"]

reserved = {v["label"].lower() for v in catalog}
for path in args.reserved_labels:
    data = json.loads(Path(path).read_text())
    if isinstance(data, dict) and "candidates" in data:
        data = [c.get("label") for c in data["candidates"].values()]
    elif isinstance(data, dict):
        data = list(data.values())
    reserved |= {str(x).lower() for x in data if x and str(x).strip()}


def vec(a):
    return emb[a] if a in emb else base_emb[a]


def cos(a, b):
    return float(np.dot(vec(a), vec(b)))


def loud(m):
    vals = [m[k]["lufs"] for k in SENTENCES if m.get(k, {}).get("lufs") is not None]
    return float(np.mean(vals)) if vals else None


def gain_for(m):
    l = loud(m)
    return None if l is None else max(-12.0, min(12.0, round((TARGET - l) * 2) / 2))


def f0_rule(f0):
    return None if f0 is None else "male" if f0 < 145 else "female" if f0 >= 175 else None


# Cross-check anchors: every voice (candidate or baseline) with an unambiguous F0 (<120 or >190 Hz).
f0_all = {**base_f0, **{v: m.get("f0_median") for v, m in metrics.items() if "error" not in m}}
ANCHORS = {v: ("male" if f < 120 else "female") for v, f in f0_all.items()
           if f and (f < 120 or f > 190) and (v in emb or v in base_emb)}


def knn_gender(v, k=7):
    near = sorted(((cos(v, a), g) for a, g in ANCHORS.items() if a != v), reverse=True)[:k]
    votes = sum(1 if g == "female" else -1 for _, g in near)
    return "female" if votes >= 3 else "male" if votes <= -3 else None


def gender(v):
    g = f0_rule(metrics[v].get("f0_median"))
    return g if g is not None and knn_gender(v) == g else None


failed = {}
for f in sorted(W.glob("synth-*.jsonl")):
    for line in f.read_text().splitlines():
        rec = json.loads(line)
        if "error" in rec:
            failed[rec["voice"]] = rec["error"]

report = {}
for v, ex in sorted(export.items()):
    base = {"voice": v, "source_id": ex["source_id"], "source_file": ex["source_file"],
            "narrator": NARRATOR_CREDIT.get(ex["narrator"], NARRATOR_FIX.get(ex["narrator"], ex["narrator"])),
            "label_base": NARRATOR_FIX.get(ex["narrator"], ex["narrator"]), "narrator_url": ex["narrator_url"], "reading": ex["reading"],
            "reading_url": ex["reading_url"], "readme_match": ex["readme_match"], "variant": ex["variant"],
            "accent": ex["accent"], "accent_raw": ex["accent_raw"], "region": ex["region"]}
    if v in failed or v not in metrics or "error" in metrics.get(v, {}):
        report[v] = {**base, "dropped": True, "reasons": [f"generation failed: {failed.get(v, 'no audio')}"]}
        continue
    m = metrics[v]; reasons = []
    w = [m["neutral"]["wer"], m["story"]["wer"]]; wmean = float(np.mean(w))
    if wmean > WER_DROP or max(w) > WER_ONE_DROP:
        reasons.append(f"intelligibility: WER {w[0]:.0%}/{w[1]:.0%}")
    for k in SENTENCES:
        s = m[k]
        if s["clipped_fraction"] > CLIP_MAX:
            reasons.append(f"clipping in {k}")
        if s["longest_gap"] > MAX_GAP:
            reasons.append(f"dropout/long pause in {k} ({s['longest_gap']}s)")
        if s["lead"] > MAX_EDGE or s["trail"] > MAX_EDGE:
            reasons.append(f"long silence at edge of {k}")
        if s["lufs"] is None or s["lufs"] < -40:
            reasons.append(f"near-silent {k}")
    wpm = float(np.mean([m["neutral"]["wpm"], m["story"]["wpm"]]))
    if wpm < WPM[0] or wpm > WPM[1]:
        reasons.append(f"speaking rate {wpm:.0f} wpm")
    near = max((cos(v, e), e) for e in exposed)
    if near[0] >= DUP:
        reasons.append(f"near-duplicate of existing voice {near[1]} ({near[0]:.3f})")
    score = wmean * 100 + 2 * max(0, max(m["neutral"]["longest_gap"], m["story"]["longest_gap"]) - 1.2) \
        + 0.05 * max(0, abs(wpm - 185) - 25) + max(0, max(m["neutral"]["lead"], m["story"]["lead"]) - 0.8)
    g = gender(v)
    report[v] = {**base, "dropped": bool(reasons), "reasons": reasons, "score": round(score, 3),
                 "wer": [round(x, 4) for x in w], "wer_mean": round(wmean, 4), "lufs": round(loud(m), 2),
                 "peak_dbfs": max(m["neutral"]["peak_dbfs"], m["story"]["peak_dbfs"]),
                 "clipped_fraction": max(m["neutral"]["clipped_fraction"], m["story"]["clipped_fraction"]),
                 "longest_gap": max(m["neutral"]["longest_gap"], m["story"]["longest_gap"]),
                 "wpm": round(wpm, 1), "f0_median": m.get("f0_median"), "f0_iqr": m.get("f0_iqr"),
                 "gender": g, "gender_estimate": g, "gender_f0_rule": f0_rule(m.get("f0_median")),
                 "gender_neighbours": knn_gender(v),
                 "gain_db": gain_for(m), "nearest_existing": [near[1], round(near[0], 4)],
                 "transcripts": {k: m[k]["transcript"] for k in SENTENCES},
                 "seconds": {k: m[k]["seconds"] for k in SENTENCES}}

# Near-duplicates among survivors: keep the better-scoring one.
alive = sorted((r for r in report.values() if not r["dropped"]), key=lambda r: r["score"])
kept = []
for r in alive:
    twin = max(((cos(r["voice"], k["voice"]), k["voice"]) for k in kept), default=(0, None))
    if twin[0] >= DUP:
        r["dropped"] = True; r["reasons"].append(f"near-duplicate of better candidate {twin[1]} ({twin[0]:.3f})")
        continue
    kept.append(r)

# Balance: alternate female and male (fewer selected so far goes next, female on ties);
# within that gender take the least-represented accent first, then the best quality score.
# Voices with no confident gender estimate come last, in score order.
# No accent may exceed accent_cap of the final set.
cap = max(1, int(args.accent_cap * args.max_keep))
pool = list(kept); chosen = []
acc = Counter(); gen = Counter()
while pool and len(chosen) < args.max_keep:
    order = sorted(("female", "male"), key=lambda g: (gen[g], g != "female"))
    pick = None
    for g in order:
        opts = [r for r in pool if r["gender"] == g and acc[r["accent"]] < cap]
        if opts:
            pick = min(opts, key=lambda r: (acc[r["accent"]], r["score"]))
            break
    if pick is None:
        opts = [r for r in pool if acc[r["accent"]] < cap]
        if not opts:
            break
        pick = min(opts, key=lambda r: (acc[r["accent"]], r["score"]))
    pool.remove(pick); chosen.append(pick); acc[pick["accent"]] += 1; gen[pick["gender"]] += 1
for r in pool:
    reason = (f"accent cap ({cap} {r['accent'] or 'unknown'} voices)" if acc[r["accent"]] >= cap
              else f"outside the {args.max_keep}-voice cap (balanced by gender and accent, then quality)")
    r["dropped"] = True; r["reasons"].append(reason)

# Names: the narrator's name, title-cased. A clip variant ("-lyrical", "2") carries its tag
# ("Padraig O'hIceadha (Lyrical)", "Diana Majlinger 2") when another clip of the same
# narrator is also selected, so the plain clip keeps the plain name. When a name is still
# taken (a live voice), add the tag, then the accent.
chosen.sort(key=lambda r: r["score"])
used = set(reserved)
per_narrator = Counter(r["label_base"] for r in chosen)


def label_options(r):
    n = r["label_base"]
    tag = None
    if r["variant"]:
        tag = f"{n} {r['variant']}" if r["variant"].isdigit() else f"{n} ({r['variant']})"
    out = [tag, n] if tag and per_narrator[n] > 1 else [n, tag] if tag else [n]
    if r["accent"]:
        out.append(f"{n} ({r['accent']})")
    out += [f"{n} {i}" for i in range(2, 9)]
    return [o.replace(",", "").replace(":", "")[:40].strip() for o in out]


for i, r in enumerate(chosen):
    r["rank"] = i + 1
    r["label"] = next(n for n in label_options(r) if n.lower() not in used)
    used.add(r["label"].lower())

selected = [r["voice"] for r in chosen]
ids = list(export)
cand_pairs = [cos(a, b) for i, a in enumerate(ids) for b in ids[i+1:] if a in emb and b in emb]
summary = {
    "candidates": len(report), "selected": len(selected),
    "dropped": {
        "generation_failed": sum(any(x.startswith("generation failed") for x in r["reasons"]) for r in report.values()),
        "intelligibility": sum(any(x.startswith("intelligibility") for x in r["reasons"]) for r in report.values()),
        "audio_faults": sum(any(x.startswith(("clipping", "dropout", "long silence", "near-silent", "speaking rate")) for x in r["reasons"]) for r in report.values()),
        "duplicate_existing": sum(any("existing voice" in x for x in r["reasons"]) for r in report.values()),
        "duplicate_candidate": sum(any("better candidate" in x for x in r["reasons"]) for r in report.values()),
        "accent_cap": sum(any(x.startswith("accent cap") for x in r["reasons"]) for r in report.values()),
        "over_cap": sum(any(x.startswith("outside the") for x in r["reasons"]) for r in report.values())},
    "passed": len(kept),
    "selected_gender": dict(Counter(str(r["gender"]) for r in chosen)),
    "selected_accent": dict(Counter(str(r["accent"]) for r in chosen).most_common()),
    "passed_gender": dict(Counter(str(r["gender"]) for r in kept)),
    "passed_accent": dict(Counter(str(r["accent"]) for r in kept).most_common()),
    "thresholds": {"wer_mean_max": WER_DROP, "wer_any_max": WER_ONE_DROP, "duplicate_cosine": DUP,
                   "candidate_pair_cosine_p99": round(float(np.percentile(cand_pairs, 99)), 4) if cand_pairs else None,
                   "candidate_pair_cosine_max": round(max(cand_pairs), 4) if cand_pairs else None,
                   "wpm_range": list(WPM), "max_gap_s": MAX_GAP, "max_edge_silence_s": MAX_EDGE,
                   "clip_fraction_max": CLIP_MAX, "accent_cap": cap},
    "exposed_baseline": len(exposed),
    "loudness_target_lufs": TARGET,
    "gender_rule": "median F0 (Praat) < 145 Hz male, >= 175 Hz female, else null; also null when 7 nearest speaker-embedding neighbours among F0-confident voices (<120 or >190 Hz) disagree",
}
first = next(iter(export.values()))
Path(args.out).write_text(json.dumps({"schema_version": 1, "generated": "2026-10-19", "source": "voice-zero",
    "source_repo": first["source_repo"], "source_commit": first["source_commit"], "license": "CC0-1.0",
    "asr": "faster-whisper 1.1.1 small.en int8 CPU, beam 1", "speaker_embedding": "microsoft/wavlm-base-plus-sv (cosine)",
    "sentences": SENTENCES, "synthesis": "production int8 ungated Pocket worker model, torch seed 20261018, one thread",
    "attribution": "Voices derived from Voice-Zero (CC0) narrator clips of LibriVox and Archive.org readings",
    "summary": summary, "selected": selected, "candidates": report}, indent=1))
print(json.dumps(summary, indent=1))
