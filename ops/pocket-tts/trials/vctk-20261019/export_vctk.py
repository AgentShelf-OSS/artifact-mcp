"""Export Pocket voice states for VCTK reference clips.

Same procedure as ../kyutai-donations-20261018/export_all.py (reproduces the shipped
andrea.safetensors, sha256 1191c25b...): gain min(-20 - RMS dBFS, 20*log10(0.9/peak)),
PCM16 at the source rate, gated english_2026-04 weights, quantize=True, truncate=True,
one torch thread.

Mounts: /work (speaker-info.txt, refs.json, refs/<spk>.wav), /out (states/, export-manifest.json).
"""
import hashlib, json, math, re, time
from pathlib import Path
import numpy as np, soundfile as sf, torch
from pocket_tts_timestamped import TTSModel, export_model_state
torch.set_num_threads(1); torch.set_num_interop_threads(1)

ZIP_URL = "https://datashare.ed.ac.uk/bitstream/handle/10283/3443/VCTK-Corpus-0.92.zip"
DOI = "https://doi.org/10.7488/ds/2645"
LICENSE = "CC-BY-4.0"
ATTRIBUTION = ("Derived from the CSTR VCTK Corpus 0.92 (Yamagishi, Veaux, MacDonald; "
               "CSTR, University of Edinburgh), CC BY 4.0")
ACCENTS = {"English": "British", "British": "British", "Scottish": "Scottish", "Irish": "Irish",
           "NorthernIrish": "Northern Irish", "Welsh": "Welsh", "American": "American", "Canadian": "Canadian",
           "Australian": "Australian", "NewZealand": "New Zealand", "SouthAfrican": "South African",
           "Indian": "Indian"}


def speaker_info(path):
    out = {}
    for line in Path(path).read_text().splitlines()[1:]:
        parts = line.split()
        if len(parts) < 4 or not re.fullmatch(r"p\d{3}|s\d", parts[0]):
            continue
        sid, age, gender, accent = parts[:4]
        rest = re.sub(r"\(.*?\)", "", " ".join(parts[4:])).split()
        if rest and rest[0] == "English" and accent in ("Australian", "NewZealand"):
            rest = rest[1:]  # "Australian English Sydney" -> region "Sydney"
        out[sid] = {"age": int(age), "gender": {"F": "female", "M": "male"}[gender], "accent_raw": accent,
                    "accent": ACCENTS.get(accent), "region": " ".join(rest) or None, "line": line.strip()}
    return out


if __name__ == "__main__":
    info = speaker_info("/work/speaker-info.txt")
    refs = json.loads(Path("/work/refs.json").read_text())
    OUT = Path("/out/states"); OUT.mkdir(parents=True, exist_ok=True)
    model = TTSModel.load_model("english_2026-04", quantize=True)
    assert model.has_voice_cloning
    rows = []
    for spk, r in sorted(refs.items()):
        if not r["ok"]:
            continue
        v = "vctk_" + spk
        src = Path("/work/refs") / f"{spk}.wav"
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
        rows.append({"voice": v, "source_collection": "CSTR VCTK Corpus 0.92", "source_id": spk,
                     "source_url": ZIP_URL, "source_doi": DOI,
                     "source_members": r["source_members"], "source_member_sha256": r["source_sha256"], "source_mic": r["mic"], "source_highpass_60hz": r["highpass_60hz"],
                     "source_sha256": hashlib.sha256(src.read_bytes()).hexdigest(),
                     "source_seconds": round(len(data)/sr, 3), "source_rms_dbfs": round(rms, 2),
                     "source_peak": round(peak, 4), "source_gain_db": round(gd, 2),
                     "embedding_sha256": hashlib.sha256(content).hexdigest(), "bytes": len(content),
                     "license": LICENSE, "attribution": ATTRIBUTION, "provenance": "VCTK (CC BY 4.0)",
                     "speaker": info.get(spk), "export_seconds": round(time.monotonic()-t0, 2)})
        print(json.dumps({"voice": v, "s": rows[-1]["export_seconds"]}), flush=True)
    json.dump({"model": "english_2026-04", "gated_revision": "19f95fe2df36e79fbd9f10008595cc4c977a0fcc",
               "quantized": True, "results": rows}, open("/out/export-manifest.json", "w"), indent=1)
    print("done", len(rows))
