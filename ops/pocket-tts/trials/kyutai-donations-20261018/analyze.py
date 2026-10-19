"""Per-voice technical metrics for the audition (no subjective judgement)."""
import json, math, re, sys
from pathlib import Path
import numpy as np, soundfile as sf, pyloudnorm as pyln, torch
from scipy.signal import resample_poly
torch.set_num_threads(2)
SENT = {
    "neutral": "Please check the schedule before Thursday. The meeting has moved to room twelve, near the north entrance.",
    "story": "Long ago, in a village by the sea, an old woman kept a lantern burning every night, waiting for a ship that never came home.",
}
NUM = {"12": "twelve", "12th": "twelfth"}
def words(t):
    t = t.lower().replace("-", " ")
    t = re.sub(r"[^a-z0-9' ]+", " ", t)
    return [NUM.get(w, w) for w in t.split()]
def wer(ref, hyp):
    r, h = words(ref), words(hyp)
    d = list(range(len(h) + 1))
    for i in range(1, len(r) + 1):
        prev, d[0] = d[0], i
        for j in range(1, len(h) + 1):
            cur = min(d[j] + 1, d[j-1] + 1, prev + (r[i-1] != h[j-1]))
            prev, d[j] = d[j], cur
    return d[len(h)] / len(r)
def frames_db(x, sr, ms=20):
    n = int(sr * ms / 1000); k = len(x) // n
    f = x[:k*n].reshape(k, n)
    return 20*np.log10(np.sqrt(np.mean(f**2, axis=1)) + 1e-9), ms/1000
def silence_stats(x, sr):
    db, step = frames_db(x, sr)
    thr = max(-50.0, float(np.percentile(db, 95)) - 35)
    voiced = db > thr
    idx = np.flatnonzero(voiced)
    if len(idx) == 0:
        return {"speech_span": 0.0, "lead": len(x)/sr, "trail": 0.0, "longest_gap": 0.0, "gaps_over_0_9s": 0}
    gaps, run = [], 0
    for v in voiced[idx[0]:idx[-1]+1]:
        if v:
            if run: gaps.append(run*step)
            run = 0
        else:
            run += 1
    return {"speech_span": round((idx[-1]-idx[0]+1)*step, 3), "lead": round(idx[0]*step, 3),
            "trail": round((len(voiced)-idx[-1]-1)*step, 3), "longest_gap": round(max(gaps, default=0.0), 3),
            "gaps_over_0_9s": int(sum(g > 0.9 for g in gaps))}
def f0_track(x, sr, fmin=60, fmax=400):
    # Decimate to 12 kHz for speed; YIN with threshold 0.15.
    x = resample_poly(x, 1, 2); sr = sr // 2
    n, hop = int(0.04*sr), int(0.01*sr)
    tmin, tmax = int(sr/fmax), int(sr/fmin)
    out = []
    for s in range(0, len(x)-n-tmax, hop):
        fr = x[s:s+n+tmax]
        if np.sqrt(np.mean(fr[:n]**2)) < 10**(-40/20): continue
        w = fr[:n]
        lag = np.lib.stride_tricks.sliding_window_view(fr, n)[:tmax+1]
        d = np.sum((lag - w)**2, axis=1)
        cm = d[1:] * np.arange(1, tmax+1) / np.maximum(np.cumsum(d[1:]), 1e-12)
        cand = np.flatnonzero(cm[tmin-1:] < 0.15)
        if len(cand) == 0: continue
        t = cand[0] + tmin - 1
        while t+1 < len(cm) and cm[t+1] < cm[t]: t += 1
        out.append(sr/(t+1))
    return out
from faster_whisper import WhisperModel
asr = WhisperModel("small.en", device="cpu", compute_type="int8", cpu_threads=2, download_root="/hf/whisper")
from transformers import AutoFeatureExtractor, WavLMForXVector
fe = AutoFeatureExtractor.from_pretrained("microsoft/wavlm-base-plus-sv", cache_dir="/hf/tf")
sv = WavLMForXVector.from_pretrained("microsoft/wavlm-base-plus-sv", cache_dir="/hf/tf").eval()
meter = pyln.Meter(24000)
jobs = json.load(open("/work/synth-jobs.json"))
res_path = Path("/out/metrics-raw.json")
res = json.loads(res_path.read_text()) if res_path.exists() else {}
emb_path = Path("/out/embeddings.npz")
embs = dict(np.load(emb_path)) if emb_path.exists() else {}
for i, job in enumerate(jobs):
    v = job["voice"]
    if v in res: continue
    files = {k: Path(f"/out/audio/{v}_{k}.wav") for k in SENT}
    if not all(p.exists() for p in files.values()):
        continue
    r = {"voice": v}
    allx, f0s = [], []
    for k, p in files.items():
        x, sr = sf.read(p, dtype="float32")
        segs, _ = asr.transcribe(str(p), language="en", beam_size=1, condition_on_previous_text=False, vad_filter=False)
        hyp = " ".join(s.text.strip() for s in segs)
        sil = silence_stats(x, sr)
        f0 = f0_track(x, sr); f0s += f0
        r[k] = {"seconds": round(len(x)/sr, 3), "transcript": hyp, "wer": round(wer(SENT[k], hyp), 4),
                "lufs": round(float(meter.integrated_loudness(x)), 2) if len(x) > sr*0.5 else None,
                "peak_dbfs": round(20*math.log10(float(np.max(np.abs(x)))+1e-9), 2),
                "clipped_fraction": round(float(np.mean(np.abs(x) >= 0.999)), 6),
                "wpm": round(len(words(SENT[k])) / max(sil["speech_span"], 0.1) * 60, 1),
                "f0_median": round(float(np.median(f0)), 1) if f0 else None, **sil}
        allx.append(x)
    r["f0_median"] = round(float(np.median(f0s)), 1) if f0s else None
    r["f0_iqr"] = [round(float(np.percentile(f0s, 25)), 1), round(float(np.percentile(f0s, 75)), 1)] if f0s else None
    y = resample_poly(np.concatenate(allx), 2, 3).astype("float32")
    with torch.no_grad():
        e = sv(**fe(y, sampling_rate=16000, return_tensors="pt")).embeddings[0]
    embs[v] = torch.nn.functional.normalize(e, dim=-1).numpy()
    res[v] = r
    print(i, v, r["neutral"]["wer"], r["story"]["wer"], r["f0_median"], flush=True)
    if i % 10 == 0:
        res_path.write_text(json.dumps(res)); np.savez(emb_path, **embs)
res_path.write_text(json.dumps(res)); np.savez(emb_path, **embs)
print("done", len(res))
