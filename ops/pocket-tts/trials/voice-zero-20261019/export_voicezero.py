"""Export Pocket voice states for the Voice-Zero narrator clips.

Same procedure as ../kyutai-donations-20261018/export_all.py and ../vctk-20261019/export_vctk.py
(reproduces the shipped andrea.safetensors, sha256 1191c25b...): gain
min(-20 - RMS dBFS, 20*log10(0.9/peak)), PCM16 at the source rate, gated english_2026-04
weights, quantize=True, truncate=True, one torch thread. The clips are used as published
(already cleaned 7-12 s readings); no trimming or joining.

Mounts: /work (sources.json, src/<file>.flac), /out (states/, export-manifest.json).
"""
import hashlib, json, math, re, time
from pathlib import Path
import numpy as np, soundfile as sf, torch
from pocket_tts_timestamped import TTSModel, export_model_state
torch.set_num_threads(1); torch.set_num_interop_threads(1)

LICENSE = "CC0-1.0"
# Accent notes in voices/README.md are the upstream author's guesses ("rough guideline").
# First part -> catalog accent; the rest is kept as the region.
ACCENTS = {"American": "American", "America": "American", "English": "British", "English?": "British",
           "Scottish": "Scottish", "Irish": "Irish", "Canadian": "Canadian", "Australian": "Australian",
           "New Zealand": "New Zealand", "German": "German", "German/Mixed bag": "German",
           "Austrian": "Austrian", "Austrian (German)": "Austrian", "Dutch": "Dutch", "Flemish": "Flemish",
           "Swedish": "Swedish", "Hungarian": "Hungarian", "French": "French", "Chinese": "Chinese"}


def accent(raw):
    parts = [p.strip() for p in raw.split(",") if p.strip()]
    head, rest = (parts[0] if parts else ""), parts[1:]
    if head == "Irish" and rest[:1] == ["Northern"]:
        return "Northern Irish", ", ".join(rest[1:]) or None
    m = [r for r in rest if r.startswith("Mother tongue:")]
    if m:  # "English, Mother tongue: Dutch"
        return m[0].split(":", 1)[1].strip(), None
    return ACCENTS.get(head), ", ".join(rest) or None


def platform(url):
    url = url or ""
    return "LibriVox" if "librivox.org" in url else "Archive.org" if "archive.org" in url else "source not given"


if __name__ == "__main__":
    src_meta = json.loads(Path("/work/sources.json").read_text())
    OUT = Path("/out/states"); OUT.mkdir(parents=True, exist_ok=True)
    model = TTSModel.load_model("english_2026-04", quantize=True)
    assert model.has_voice_cloning
    rows = []
    for r in src_meta["candidates"]:
        v = r["voice"]
        src = Path("/work/src") / r["file"]
        content = src.read_bytes()
        if hashlib.sha256(content).hexdigest() != r["sha256"]:
            raise SystemExit(f"source checksum mismatch for {v}")
        t0 = time.monotonic()
        data, sr = sf.read(src, dtype="float32")
        if data.ndim > 1:
            data = data.mean(axis=1)
        rms = 20*math.log10(max(float(np.sqrt(np.mean(data**2))), 1e-9)); peak = max(float(np.max(np.abs(data))), 1e-9)
        gd = min(-20 - rms, 20*math.log10(0.9/peak))
        tmp = Path("/tmp/norm.wav"); sf.write(tmp, data*10**(gd/20), sr, subtype="PCM_16")
        dest = OUT / (v + ".safetensors")
        export_model_state(model.get_state_for_audio_prompt(tmp, truncate=True), dest)
        state = dest.read_bytes()
        acc, region = accent(r["accent_raw"])
        rows.append({"voice": v, "source_collection": "Voice-Zero", "source_repo": src_meta["repo"],
                     "source_commit": src_meta["commit"], "source_id": r["stem"], "source_file": r["file"],
                     "source_url": r["source_url"], "source_sha256": r["sha256"], "source_git_blob": r["git_blob_sha1"],
                     "source_sample_rate": sr, "source_seconds": round(len(data)/sr, 3),
                     "source_rms_dbfs": round(rms, 2), "source_peak": round(peak, 4), "source_gain_db": round(gd, 2),
                     "narrator": r["narrator"], "narrator_url": r["reader_url"], "reading": r["work"],
                     "reading_url": r["work_url"], "readme_match": r["readme_match"], "variant": r["variant"],
                     "accent_raw": r["accent_raw"], "accent": acc, "region": region,
                     "embedding_sha256": hashlib.sha256(state).hexdigest(), "bytes": len(state),
                     "license": LICENSE,
                     "attribution": f"Voice-Zero (CC0) clip of {r['narrator_raw']} reading {r['work']} ({platform(r['work_url'])})",
                     "provenance": "Voice-Zero (CC0)", "export_seconds": round(time.monotonic()-t0, 2)})
        print(json.dumps({"voice": v, "s": rows[-1]["export_seconds"], "sr": sr, "sec": rows[-1]["source_seconds"]}), flush=True)
    json.dump({"model": "english_2026-04", "gated_revision": "19f95fe2df36e79fbd9f10008595cc4c977a0fcc",
               "quantized": True, "source_repo": src_meta["repo"], "source_commit": src_meta["commit"],
               "results": rows}, open("/out/export-manifest.json", "w"), indent=1)
    print("done", len(rows))
