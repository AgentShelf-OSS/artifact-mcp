"""Build one 10-15 s reference clip per VCTK speaker from clean utterances.

Usage: build_refs.py <src-dir> <out-dir>
  src-dir: extracted zip members (wav48_silence_trimmed/<spk>/<spk>_<nnn>_mic{1,2}.flac, txt/...)
  out-dir: refs/<spk>.wav (float32, 48 kHz mono) and refs.json (choices and per-clip metrics)

Per utterance: peak, clipped samples, noise floor (10th percentile of 20 ms frame RMS),
speech level (95th percentile) and their difference as an SNR estimate.
Microphone: mic2 (MKH 800) unless mic1 is better by more than 3 dB median SNR; the
corpus notes mic1 (DPA 4035) has low-frequency noise. Speakers without mic2 (p280,
p315) use mic1 with a 60 Hz 4th-order high-pass.
Utterances: no clipping (|x| >= 0.999), ranked by SNR, added until the clip reaches
10 s (3-5 utterances, at most 15 s; two suffice when they already reach 10 s). Edge silence is cut to 0.2 s per side so joins
keep natural room tone (gap <= 0.4 s), with 10 ms fades. No loudness change here: the
export step applies the standard gain (min(-20 - RMS dBFS, 20 log10(0.9/peak))).
"""
import hashlib, json, re, sys
from collections import defaultdict
from pathlib import Path
import numpy as np, soundfile as sf
from scipy.signal import butter, sosfiltfilt

SRC, OUT = Path(sys.argv[1]), Path(sys.argv[2])
(OUT / "refs").mkdir(parents=True, exist_ok=True)
SR = 48000
MIN_S, MAX_S, MIN_N, MAX_N = 10.0, 15.0, 3, 5
EDGE = 0.2


def frames_db(x, ms=20):
    n = int(SR * ms / 1000); k = len(x) // n
    return 20 * np.log10(np.sqrt(np.mean(x[:k * n].reshape(k, n) ** 2, axis=1)) + 1e-9)


def trim(x):
    db = frames_db(x)
    voiced = np.flatnonzero(db > max(float(np.percentile(db, 95)) - 35, -60))
    n = int(SR * 0.02); pad = int(SR * EDGE)
    if len(voiced) == 0:
        return x
    a, b = max(0, voiced[0] * n - pad), min(len(x), (voiced[-1] + 1) * n + pad)
    return x[a:b]


def metrics(x):
    db = frames_db(x)
    return {"seconds": round(len(x) / SR, 3), "peak": round(float(np.max(np.abs(x))), 4),
            "clipped": int(np.sum(np.abs(x) >= 0.999)),
            "noise_db": round(float(np.percentile(db, 10)), 1), "speech_db": round(float(np.percentile(db, 95)), 1),
            "snr_db": round(float(np.percentile(db, 95) - np.percentile(db, 10)), 1)}


HP = butter(4, 60, "highpass", fs=SR, output="sos")
files = defaultdict(lambda: defaultdict(dict))
for p in sorted((SRC / "wav48_silence_trimmed").glob("*/*.flac")):
    m = re.match(r"(\w+?)_(\d+)_(mic[12])$", p.stem)
    files[m.group(1)][m.group(2)][m.group(3)] = p

report = {}
for spk, utts in sorted(files.items()):
    per = {}
    for num, mics in utts.items():
        per[num] = {}
        for mic, p in mics.items():
            x, sr = sf.read(p, dtype="float64")
            assert sr == SR, (p, sr)
            if x.ndim > 1:
                x = x.mean(axis=1)
            per[num][mic] = metrics(x)
    snr = {mic: float(np.median([v[mic]["snr_db"] for v in per.values() if mic in v]))
           for mic in ("mic1", "mic2") if any(mic in v for v in per.values())}
    mic = "mic2" if "mic2" in snr and not ("mic1" in snr and snr["mic1"] > snr["mic2"] + 3) else "mic1"
    highpass = mic == "mic1"
    ranked = sorted((n for n in per if mic in per[n] and per[n][mic]["clipped"] == 0),
                    key=lambda n: -per[n][mic]["snr_db"])
    chosen, parts, total = [], [], 0.0
    for n in ranked:
        x, _ = sf.read(utts[n][mic], dtype="float64")
        if x.ndim > 1:
            x = x.mean(axis=1)
        if highpass:
            x = sosfiltfilt(HP, x)
        x = trim(x)
        if total + len(x) / SR > MAX_S:
            continue
        f = int(SR * 0.01); ramp = np.linspace(0, 1, f)
        x = x.copy(); x[:f] *= ramp; x[-f:] *= ramp[::-1]
        parts.append(x); chosen.append(n); total += len(x) / SR
        if len(chosen) >= MAX_N or (total >= MIN_S and len(chosen) >= MIN_N):
            break
    # Two utterances are enough when they already reach MIN_S (slow speakers, long sentences).
    ok = total >= MIN_S - 1.0 and (len(chosen) >= MIN_N or (total >= MIN_S and len(chosen) >= 2))
    ref = np.concatenate(parts) if parts else np.zeros(1)
    if ok:
        sf.write(OUT / "refs" / f"{spk}.wav", ref.astype("float32"), SR, subtype="FLOAT")
    texts = {}
    for n in chosen:
        t = SRC / "txt" / spk / f"{spk}_{n}.txt"
        texts[n] = t.read_text().strip() if t.exists() else None
    report[spk] = {"mic": mic, "highpass_60hz": highpass, "mic_median_snr_db": snr, "utterances": chosen,
                   "texts": texts, "seconds": round(total, 2), "ok": ok,
                   "ref": metrics(ref) if parts else None, "candidates": per,
                   "source_members": [str(utts[n][mic].relative_to(SRC)) for n in chosen],
                   "source_sha256": [hashlib.sha256(utts[n][mic].read_bytes()).hexdigest() for n in chosen]}
    print(spk, mic, {k: round(v, 1) for k, v in snr.items()}, chosen, round(total, 1), "ok" if ok else "SHORT", flush=True)

(OUT / "refs.json").write_text(json.dumps(report, indent=1))
print("refs", sum(r["ok"] for r in report.values()), "of", len(report))
