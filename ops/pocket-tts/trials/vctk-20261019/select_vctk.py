"""Filter, de-duplicate, balance, and name the VCTK voice candidates.

Usage: select_vctk.py <vctk-work-dir> <donations-out-dir> <catalog.json> <kyutai-voice-metrics.json> <out.json>
                      [--reserved-labels file.json] [--max-keep 50]

Inputs (vctk-work-dir/out): metrics-raw.json, embeddings.npz, f0.json, export-manifest.json,
synth-*.jsonl. donations-out-dir: embeddings.npz holds the same-seed baseline for every
exposed voice (built-ins, extras, kept donations). catalog.json: the installed catalog
(exposed ids and labels). kyutai-voice-metrics.json: names of Kyutai audition candidates,
which stay reserved so a later Keep there cannot clash with a VCTK name.

Thresholds are the donation audition's (see ../kyutai-donations-20261018/README.md).
Gender and accent come from VCTK speaker-info.txt. F0 is a sanity check only.
Nothing here is a listening judgement; Neil decides in the audition.
"""
import argparse, json
from collections import Counter
from pathlib import Path
import numpy as np

ap = argparse.ArgumentParser()
ap.add_argument("work"); ap.add_argument("donations_out"); ap.add_argument("catalog"); ap.add_argument("kyutai_metrics")
ap.add_argument("out"); ap.add_argument("--reserved-labels"); ap.add_argument("--max-keep", type=int, default=50)
args = ap.parse_args()
W = Path(args.work) / "out"

WER_DROP, WER_ONE_DROP = 0.12, 0.25
MAX_GAP, MAX_EDGE, WPM = 2.0, 1.5, (120, 250)
CLIP_MAX = 0.001
DUP = 0.975
TARGET = -18.63          # loudness target from the donation audition (median LUFS + gain of exposed voices)
ACCENT_CAP = 0.25        # of the selected set
SENTENCES = {"neutral": "Please check the schedule before Thursday. The meeting has moved to room twelve, near the north entrance.",
             "story": "Long ago, in a village by the sea, an old woman kept a lantern burning every night, waiting for a ship that never came home."}

# Friendly first names by accent and gender. Picked in order, skipping any name already
# used anywhere in the catalog, the Kyutai audition, or earlier in this list.
NAMES = {
    ("British", "female"): ["Harriet", "Imogen", "Poppy", "Beatrice", "Rosie", "Philippa", "Florence", "Georgia", "Matilda", "Esme", "Hattie", "Lydia", "Penny", "Tamsin", "Verity", "Felicity", "Jemima", "Abigail", "Sophie", "Annabel", "Bryony", "Charlotte", "Daisy", "Eleanor", "Frances", "Gemma", "Holly", "Isobel", "Lottie"],
    ("British", "male"): ["Alfie", "Rupert", "Oliver", "Toby", "Freddie", "Hugo", "Archie", "Barnaby", "Edmund", "Giles", "Henry", "Jasper", "Miles", "Nigel", "Rory", "Simon", "Tristan", "Ben", "Dominic", "Harry"],
    ("Scottish", "female"): ["Fiona", "Isla", "Morag", "Eilidh", "Kirsty", "Ailsa", "Mhairi", "Catriona", "Iona", "Shona", "Lorna", "Heather", "Elspeth", "Skye", "Rhona"],
    ("Scottish", "male"): ["Callum", "Angus", "Hamish", "Euan", "Fraser", "Alasdair", "Struan", "Lachlan", "Dougal", "Finlay", "Ruaridh", "Gregor", "Ewan", "Murray", "Craig", "Duncan"],
    ("Irish", "female"): ["Siobhan", "Aoife", "Niamh", "Orla", "Saoirse", "Roisin", "Grainne", "Clodagh", "Aisling", "Mairead"],
    ("Irish", "male"): ["Ciaran", "Cormac", "Eoin", "Padraig", "Declan", "Ronan", "Fergal", "Oisin"],
    ("Northern Irish", "female"): ["Caoimhe", "Bronagh", "Deirdre", "Una", "Maeve", "Sinead"],
    ("Northern Irish", "male"): ["Seamus", "Colm", "Eamon", "Niall"],
    ("Welsh", "female"): ["Rhian", "Cerys", "Bethan", "Ffion", "Seren"],
    ("Welsh", "male"): ["Gareth", "Rhys", "Dafydd", "Iwan"],
    ("American", "female"): ["Madison", "Savannah", "Brooke", "Kayla", "Shelby", "Taylor", "Megan", "Ashley", "Courtney", "Lindsey", "Brittany", "Jenna", "Kelsey", "Morgan", "Paige", "Whitney", "Danielle", "Haley", "Lauren", "Melissa"],
    ("American", "male"): ["Tyler", "Cody", "Dustin", "Garrett", "Travis", "Wyatt", "Brandon", "Chase", "Logan", "Hunter"],
    ("Canadian", "female"): ["Chantal", "Kendra", "Shannon", "Brianna", "Tessa", "Raina", "Marissa", "Joelle", "Leah", "Natalie"],
    ("Canadian", "male"): ["Mathieu", "Brayden", "Kyle", "Trevor", "Gordie", "Connor"],
    ("South African", "female"): ["Thandi", "Annelie", "Lerato", "Marike", "Zanele"],
    ("South African", "male"): ["Pieter", "Sipho", "Johan", "Thabo"],
    ("Australian", "female"): ["Matilda", "Kylie", "Sheree"],
    ("Australian", "male"): ["Bruce", "Darcy", "Lachie", "Jarrah"],
    ("New Zealand", "female"): ["Aroha", "Kiri", "Mere"],
    ("New Zealand", "male"): ["Nikau", "Tane"],
    ("Indian", "female"): ["Priya", "Ananya", "Kavya", "Meera"],
    ("Indian", "male"): ["Vikram", "Arjun", "Rohan", "Sanjay", "Aditya"],
    (None, "female"): ["Amelie", "Margaux", "Celine", "Ines", "Juliette"],
    (None, "male"): ["Laurent", "Etienne", "Bastien", "Mathis"],
}

metrics = json.loads((W / "metrics-raw.json").read_text())
emb = dict(np.load(W / "embeddings.npz"))
base_emb = dict(np.load(Path(args.donations_out) / "embeddings.npz"))
F0 = json.loads((W / "f0.json").read_text())
export = {r["voice"]: r for r in json.loads((W / "export-manifest.json").read_text())["results"]}
catalog = json.loads(Path(args.catalog).read_text())["voices"]
exposed = [v["id"] for v in catalog]
missing = [v for v in exposed if v not in base_emb]
if missing:
    raise SystemExit(f"no baseline embedding for exposed voices: {missing}")
for v, f in F0.items():
    if v in metrics:
        metrics[v]["f0_median"], metrics[v]["f0_iqr"] = f["f0_median"], f["f0_iqr"]

reserved = {v["label"].split()[0].lower() for v in catalog}
kyutai = json.loads(Path(args.kyutai_metrics).read_text())
for v in kyutai["selected"]:
    sid = kyutai["candidates"][v]["source_id"].removesuffix("_enhanced")
    reserved.add(sid.replace("-", "_").split("_")[0].lower())
if args.reserved_labels:
    reserved |= {str(x).split()[0].lower() for x in json.loads(Path(args.reserved_labels).read_text()) if str(x).strip()}


def cos(a, b):
    ea = emb.get(a, base_emb.get(a)); eb = emb.get(b, base_emb.get(b))
    return float(np.dot(ea, eb))


def loud(m):
    vals = [m[k]["lufs"] for k in SENTENCES if m.get(k, {}).get("lufs") is not None]
    return float(np.mean(vals)) if vals else None


def gain_for(m):
    l = loud(m)
    return None if l is None else max(-12.0, min(12.0, round((TARGET - l) * 2) / 2))


def f0_rule(f0):
    return None if f0 is None else "male" if f0 < 145 else "female" if f0 >= 175 else None


failed = {}
for f in sorted(W.glob("synth-*.jsonl")):
    for line in f.read_text().splitlines():
        rec = json.loads(line)
        if "error" in rec:
            failed[rec["voice"]] = rec["error"]

report = {}
for v, ex in sorted(export.items()):
    spk = ex["speaker"] or {}
    base = {"voice": v, "source_id": ex["source_id"], "gender": spk.get("gender"), "accent": spk.get("accent"),
            "accent_raw": spk.get("accent_raw"), "region": spk.get("region"), "age": spk.get("age")}
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
    f0g = f0_rule(m.get("f0_median"))
    report[v] = {**base, "dropped": bool(reasons), "reasons": reasons, "score": round(score, 3),
                 "wer": [round(x, 4) for x in w], "wer_mean": round(wmean, 4), "lufs": round(loud(m), 2),
                 "peak_dbfs": max(m["neutral"]["peak_dbfs"], m["story"]["peak_dbfs"]),
                 "clipped_fraction": max(m["neutral"]["clipped_fraction"], m["story"]["clipped_fraction"]),
                 "longest_gap": max(m["neutral"]["longest_gap"], m["story"]["longest_gap"]),
                 "wpm": round(wpm, 1), "f0_median": m.get("f0_median"), "f0_iqr": m.get("f0_iqr"),
                 "gender_estimate": spk.get("gender"), "gender_f0_rule": f0g,
                 "f0_check": "agrees" if f0g == spk.get("gender") else "ambiguous" if f0g is None else "disagrees",
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

# Balance: alternate genders (fewer selected so far goes next, female on ties); within
# that gender take the least-represented accent first, then the best quality score.
# No accent may exceed ACCENT_CAP of the final set.
cap = max(1, int(ACCENT_CAP * args.max_keep))
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

chosen.sort(key=lambda r: r["score"])
used = set(reserved)
for i, r in enumerate(chosen):
    r["rank"] = i + 1
    names = NAMES.get((r["accent"], r["gender"]), []) + NAMES[(None, r["gender"])] + \
        [n for (a, g), ns in NAMES.items() if g == r["gender"] for n in ns]
    r["label"] = next(n for n in names if n.lower() not in used)
    used.add(r["label"].lower())

selected = [r["voice"] for r in chosen]
cand_pairs = [cos(a, b) for i, a in enumerate(export) for b in list(export)[i+1:] if a in emb and b in emb]
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
    "selected_gender": dict(Counter(r["gender"] for r in chosen)),
    "selected_accent": dict(Counter(str(r["accent"]) for r in chosen).most_common()),
    "passed_gender": dict(Counter(r["gender"] for r in kept)),
    "passed_accent": dict(Counter(str(r["accent"]) for r in kept).most_common()),
    "f0_check_selected": dict(Counter(r["f0_check"] for r in chosen)),
    "thresholds": {"wer_mean_max": WER_DROP, "wer_any_max": WER_ONE_DROP, "duplicate_cosine": DUP,
                   "candidate_pair_cosine_p99": round(float(np.percentile(cand_pairs, 99)), 4) if cand_pairs else None,
                   "candidate_pair_cosine_max": round(max(cand_pairs), 4) if cand_pairs else None,
                   "wpm_range": list(WPM), "max_gap_s": MAX_GAP, "max_edge_silence_s": MAX_EDGE,
                   "clip_fraction_max": CLIP_MAX, "accent_cap": cap},
    "loudness_target_lufs": TARGET,
    "gender_rule": "VCTK speaker-info.txt; F0 (Praat median, <145 male, >=175 female) recorded as a sanity check",
}
Path(args.out).write_text(json.dumps({"schema_version": 1, "generated": "2026-10-19", "source": "vctk",
    "asr": "faster-whisper 1.1.1 small.en int8 CPU, beam 1", "speaker_embedding": "microsoft/wavlm-base-plus-sv (cosine)",
    "sentences": SENTENCES, "synthesis": "production int8 ungated Pocket worker model, torch seed 20261018, one thread",
    "attribution": "Voices derived from the VCTK corpus, CSTR, University of Edinburgh, CC BY 4.0",
    "summary": summary, "selected": selected, "candidates": report}, indent=1))
print(json.dumps(summary, indent=1))
