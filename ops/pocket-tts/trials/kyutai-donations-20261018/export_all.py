"""Export Pocket voice states for Kyutai voice donations (procedure reproduces the
first donated batch exactly: andrea sha256 1191c25b...).
Gain: min(-20 - RMS dBFS, 20*log10(0.9/peak)); PCM16 at source rate; quantize=True;
truncate=True; one torch thread."""
import hashlib, json, math, re, sys, time
from pathlib import Path
import numpy as np, soundfile as sf, torch
from pocket_tts_timestamped import TTSModel, export_model_state
torch.set_num_threads(1); torch.set_num_interop_threads(1)
REV = "323332d33f997de8394f24a193e1a76df720e01a"
CONSENT = "https://kyutai.org/legal/Terms%20of%20Use%20-%20Unmute%20Voice%20Donation%20Project%20v1.pdf"
ACCEPTED = {"Andrea_enhanced", "Darius_enhanced", "Hannah_enhanced", "Midlands_Bedfordshire_Dialect_enhanced"}
RESERVED = set(json.load(open("/work/reserved.json")))
OUT = Path("/out/states"); OUT.mkdir(exist_ok=True)
def vid(source_id):
    base = source_id[:-len("_enhanced")].lower()
    base = re.sub(r"[^a-z0-9]+", "_", base).strip("_")
    if not base or not base[0].isalpha():
        base = "donor_" + base
    return base[:40].rstrip("_")
model = TTSModel.load_model("english_2026-04", quantize=True)
assert model.has_voice_cloning
rows, used = [], set(RESERVED)
for line in open("/work/enhanced.tsv"):
    path, lfs_sha, size = line.split()
    source_id = Path(path).stem
    if source_id in ACCEPTED:
        continue
    v = vid(source_id); n = 2
    while v in used:
        v = f"{vid(source_id)[:37]}_{n}"; n += 1
    used.add(v)
    src = Path("/work/source") / Path(path).name
    t0 = time.monotonic()
    data, sr = sf.read(src, dtype="float32")
    if data.ndim > 1:
        data = data.mean(axis=1)
    rms = 20*math.log10(max(float(np.sqrt(np.mean(data**2))), 1e-9)); peak = max(float(np.max(np.abs(data))), 1e-9)
    gd = min(-20 - rms, 20*math.log10(0.9/peak))
    tmp = Path("/tmp/norm.wav"); sf.write(tmp, data*10**(gd/20), sr, subtype="PCM_16")
    dest = OUT / (v + ".safetensors")
    export_model_state(model.get_state_for_audio_prompt(tmp, truncate=True), dest)
    content = dest.read_bytes()
    rows.append({"voice": v, "source_collection": "kyutai/tts-voices/voice-donations", "source_id": source_id,
                 "source_revision": REV, "source_sha256": lfs_sha, "source_seconds": round(len(data)/sr, 3),
                 "source_rms_dbfs": round(rms, 2), "source_peak": round(peak, 4),
                 "source_gain_db": round(gd, 2), "embedding_sha256": hashlib.sha256(content).hexdigest(),
                 "bytes": len(content), "license": "CC0-1.0", "consent_terms": CONSENT,
                 "export_seconds": round(time.monotonic()-t0, 2)})
    print(json.dumps({"voice": v, "s": rows[-1]["export_seconds"]}), flush=True)
json.dump({"model": "english_2026-04", "gated_revision": "19f95fe2df36e79fbd9f10008595cc4c977a0fcc", "quantized": True,
           "results": rows}, open("/out/export-manifest.json", "w"), indent=1)
print("done", len(rows))
