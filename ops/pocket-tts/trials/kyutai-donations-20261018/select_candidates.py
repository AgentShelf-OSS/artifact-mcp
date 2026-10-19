"""Rank and filter donated-voice candidates from technical metrics.

Inputs (from the VM310 trial dir): metrics-raw.json, embeddings.npz,
export-manifest.json, synth-jobs.json. Output: voice-metrics.json report.
Nothing here is a listening judgement; Neil decides in the audition.
"""
import json, math, sys
from pathlib import Path
import numpy as np

W = Path(sys.argv[1]); OUT = Path(sys.argv[2])
MAX_KEEP = 80
WER_DROP = 0.12          # mean WER over both sentences
WER_ONE_DROP = 0.25      # any single sentence
# Limits sit just outside the envelope of the 35 accepted voices (max gap 1.9 s,
# max lead 1.24 s, 171-245 wpm, max distinct-pair speaker cosine 0.9725).
MAX_GAP, MAX_EDGE, WPM = 2.0, 1.5, (120, 250)
DUP = 0.975
EXPOSED_GAINS = {  # current artifact-mcp gains (built-ins + POCKET_TTS_EXTRA_VOICES)
    "alba": 0.0, "marius": 8.5, "javert": 7.0, "jean": 0.0, "cosette": 11.5, "eponine": 1.5, "fantine": 1.0,
    "azelma": 1.5, "anna": 1.0, "bill_boerst": 8.0, "caro_davy": 7.0, "charles": 3.5, "eve": 2.0, "george": 1.0,
    "jane": 1.5, "mary": 1.5, "michael": 2.5, "paul": 0.5, "peter_yearsley": 10.5, "stuart_bell": 12.0, "vera": 2.5,
    "estelle": 2.5, "giovanni": 8.0, "juergen": 9.0, "lola": 2.0, "rafael": 8.5,
    "andrea": 6.0, "darius": 3.5, "hannah": 3.0, "bedfordshire": 4.5}

metrics = json.loads((W / "metrics-raw.json").read_text())
emb = dict(np.load(W / "embeddings.npz"))
export = {r["voice"]: r for r in json.loads((W / "export-manifest.json").read_text())["results"]}
jobs = json.loads((W / "synth-jobs.json").read_text())
F0 = json.loads((W / "f0.json").read_text())  # Praat autocorrelation pitch, 75-500 Hz
for v, f in F0.items():
    if v in metrics:
        metrics[v]["f0_median"], metrics[v]["f0_iqr"] = f["f0_median"], f["f0_iqr"]
kind = {j["voice"]: j["kind"] for j in jobs}

def loud(m):
    vals = [m[k]["lufs"] for k in ("neutral", "story") if m.get(k, {}).get("lufs") is not None]
    return float(np.mean(vals)) if vals else None

# Loudness target: the playback loudness the existing gains aim at.
offsets = [loud(metrics[v]) + g for v, g in EXPOSED_GAINS.items() if v in metrics and loud(metrics[v]) is not None]
TARGET = float(np.median(offsets))
residual = [abs(loud(metrics[v]) + g - TARGET) for v, g in EXPOSED_GAINS.items() if v in metrics]

def gain_for(m):
    l = loud(m)
    if l is None:
        return None
    g = TARGET - l  # the Listen player's -3 dB limiter catches peaks, as for existing gains
    return max(-12.0, min(12.0, round(g * 2) / 2))

def f0_gender(f0):
    if f0 is None:
        return None
    if f0 < 145:
        return "male"
    if f0 >= 175:
        return "female"
    return None  # ambiguous: leave for Neil

# Cross-check: anchors are voices whose F0 is unambiguous (<120 or >190 Hz).
ANCHORS = {v: ("male" if m["f0_median"] < 120 else "female") for v, m in metrics.items()
           if v in emb and m.get("f0_median") and (m["f0_median"] < 120 or m["f0_median"] > 190)}
def knn_gender(v, k=7):
    near = sorted(((float(np.dot(emb[v], emb[a])), g) for a, g in ANCHORS.items() if a != v), reverse=True)[:k]
    votes = sum(1 if g == "female" else -1 for _, g in near)
    return "female" if votes >= 3 else "male" if votes <= -3 else None

def gender(v):
    g = f0_gender(metrics[v].get("f0_median"))
    if g is None or v not in emb:
        return g
    return g if knn_gender(v) == g else None

def cos(a, b):
    return float(np.dot(emb[a], emb[b]))

exposed = [v for v in EXPOSED_GAINS if v in emb]
cands = [v for v in metrics if kind.get(v) == "candidate"]
failed = {}
for line in [l for f in sorted(W.glob("synth-*.jsonl")) for l in f.read_text().splitlines()]:
    rec = json.loads(line)
    if "error" in rec and kind.get(rec["voice"]) == "candidate":
        failed[rec["voice"]] = rec["error"]

# Calibrate the duplicate threshold from pairs that are known to be one donor.
same_donor = [("gonzalo", "gonzalo_1"), ("oldnerd", "oldnerd2"), ("oldnerd", "oldnerd3"), ("oldnerd2", "oldnerd3"),
              ("hielos_1", "hielos_2"), ("enrique", "enrique_spanish"), ("andrea_spanish", "andrea")]
known = {f"{a}~{b}": round(cos(a, b), 4) for a, b in same_donor if a in emb and b in emb}
cand_pairs = [cos(a, b) for i, a in enumerate(cands) for b in cands[i+1:] if a in emb and b in emb]
p99 = float(np.percentile(cand_pairs, 99)) if cand_pairs else 0.99
existing_pairs = [cos(a, b) for i, a in enumerate(exposed) for b in exposed[i+1:]]

report = {}
for v in cands:
    m = metrics[v]; reasons = []
    if "error" in m:
        report[v] = {"voice": v, "dropped": True, "reasons": [m["error"]]}
        continue
    w = [m["neutral"]["wer"], m["story"]["wer"]]
    wmean = float(np.mean(w))
    if wmean > WER_DROP or max(w) > WER_ONE_DROP:
        reasons.append(f"intelligibility: WER {w[0]:.0%}/{w[1]:.0%}")
    for k in ("neutral", "story"):
        s = m[k]
        if s["clipped_fraction"] > 0.001:
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
    near_exp = max(((cos(v, e), e) for e in exposed), default=(0, None))
    if near_exp[0] >= DUP:
        reasons.append(f"near-duplicate of existing voice {near_exp[1]} ({near_exp[0]:.3f})")
    # Quality score: lower is better (WER dominates; mild penalties for pauses/rate extremes).
    score = wmean * 100 + 2 * max(0, max(m["neutral"]["longest_gap"], m["story"]["longest_gap"]) - 1.2) \
        + 0.05 * max(0, abs(wpm - 185) - 25) + max(0, max(m["neutral"]["lead"], m["story"]["lead"]) - 0.8)
    ex = export[v]
    report[v] = {
        "voice": v, "source_id": ex["source_id"], "dropped": bool(reasons), "reasons": reasons,
        "score": round(score, 3), "wer": [round(x, 4) for x in w], "wer_mean": round(wmean, 4),
        "lufs": round(loud(m), 2), "peak_dbfs": max(m["neutral"]["peak_dbfs"], m["story"]["peak_dbfs"]),
        "clipped_fraction": max(m["neutral"]["clipped_fraction"], m["story"]["clipped_fraction"]),
        "longest_gap": max(m["neutral"]["longest_gap"], m["story"]["longest_gap"]),
        "wpm": round(wpm, 1), "f0_median": m["f0_median"], "f0_iqr": m["f0_iqr"],
        "gender_estimate": gender(v), "gender_f0_rule": f0_gender(m["f0_median"]), "gender_neighbours": knn_gender(v), "gain_db": gain_for(m),
        "nearest_existing": [near_exp[1], round(near_exp[0], 4)],
        "transcripts": {k: m[k]["transcript"] for k in ("neutral", "story")},
        "seconds": {k: m[k]["seconds"] for k in ("neutral", "story")},
    }

for v, err in failed.items():
    report[v] = {"voice": v, "source_id": export[v]["source_id"], "dropped": True,
                 "reasons": [f"generation failed: {err}"]}

# Near-duplicates among survivors: keep the better-scoring one.
alive = sorted((r for r in report.values() if not r["dropped"]), key=lambda r: r["score"])
kept = []
for r in alive:
    twin = max(((cos(r["voice"], k["voice"]), k["voice"]) for k in kept), default=(0, None))
    if twin[0] >= DUP:
        r["dropped"] = True; r["reasons"].append(f"near-duplicate of better candidate {twin[1]} ({twin[0]:.3f})")
        continue
    kept.append(r)
for i, r in enumerate(kept):
    r["rank"] = i + 1
# The donor pool is mostly male. Keep every passing female or unsure voice, then
# fill to MAX_KEEP with the best-scoring male voices.
priority = [r for r in kept if r["gender_estimate"] != "male"]
rest = [r for r in kept if r["gender_estimate"] == "male"]
chosen = {r["voice"] for r in (priority + rest)[:MAX_KEEP]}
for r in kept:
    if r["voice"] not in chosen:
        r["dropped"] = True; r["reasons"].append(f"outside the {MAX_KEEP}-voice cap (male voices ranked by quality score)")

existing = {}
for v in metrics:
    if kind.get(v) == "candidate" or "error" in metrics[v]:
        continue
    m = metrics[v]
    existing[v] = {"voice": v, "kind": kind[v], "wer": [m["neutral"]["wer"], m["story"]["wer"]],
                   "lufs": round(loud(m), 2), "f0_median": m["f0_median"], "gender_estimate": gender(v),
                   "wpm": round(float(np.mean([m["neutral"]["wpm"], m["story"]["wpm"]])), 1),
                   "current_gain_db": EXPOSED_GAINS.get(v), "suggested_gain_db": gain_for(m)}

selected = [r["voice"] for r in kept if r["voice"] in chosen]
summary = {
    "candidates": len(report), "selected": len(selected),
    "dropped": {"intelligibility": sum(any(x.startswith("intelligibility") for x in r["reasons"]) for r in report.values()),
                "audio_faults": sum(any(x.startswith(("clipping", "dropout", "long silence", "near-silent", "speaking rate")) for x in r["reasons"]) for r in report.values()),
                "duplicate_existing": sum(any("existing voice" in x for x in r["reasons"]) for r in report.values()),
                "duplicate_candidate": sum(any("better candidate" in x for x in r["reasons"]) for r in report.values()),
                "over_cap": sum(any(x.startswith("outside the") for x in r["reasons"]) for r in report.values()),
                "generation_failed": sum(any(x.startswith("generation failed") for x in r["reasons"]) for r in report.values())},
    "selected_gender_estimates": {str(g): sum(report[v]["gender_estimate"] == g for v in [r["voice"] for r in kept if r["voice"] in chosen]) for g in ("female", "male", None)},
    "thresholds": {"wer_mean_max": WER_DROP, "wer_any_max": WER_ONE_DROP, "duplicate_cosine": round(DUP, 4),
                   "candidate_pair_cosine_p99": round(p99, 4), "known_same_donor_cosine": known,
                   "existing_pair_cosine_max": round(max(existing_pairs), 4),
                   "wpm_range": list(WPM), "max_gap_s": MAX_GAP, "max_edge_silence_s": MAX_EDGE, "clip_fraction_max": 0.001},
    "loudness_target_lufs": round(TARGET, 2), "existing_gain_residual_db_median": round(float(np.median(residual)), 2),
    "gender_rule": "median F0 (Praat) < 145 Hz male, >= 175 Hz female, else null; also null when 7 nearest speaker-embedding neighbours among F0-confident voices (<120 or >190 Hz) disagree",
}
OUT.write_text(json.dumps({"schema_version": 1, "generated": "2026-10-18",
    "asr": "faster-whisper 1.1.1 small.en int8 CPU, beam 1", "speaker_embedding": "microsoft/wavlm-base-plus-sv (cosine)",
    "sentences": {"neutral": "Please check the schedule before Thursday. The meeting has moved to room twelve, near the north entrance.",
                  "story": "Long ago, in a village by the sea, an old woman kept a lantern burning every night, waiting for a ship that never came home."},
    "synthesis": "production int8 ungated Pocket worker model, torch seed 20261018, one thread",
    "summary": summary, "selected": selected, "candidates": report, "existing": existing}, indent=1))
print(json.dumps(summary, indent=1))
