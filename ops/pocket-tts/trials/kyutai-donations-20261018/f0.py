"""Median F0 per voice with Praat (autocorrelation, 75-500 Hz) over both samples."""
import json, sys
from pathlib import Path
import numpy as np, parselmouth
jobs = json.load(open("/work/synth-jobs.json"))
out = {}
for j in jobs:
    v = j["voice"]; vals = []
    for k in ("neutral", "story"):
        p = Path(f"/out/audio/{v}_{k}.wav")
        if not p.exists():
            break
        pitch = parselmouth.Sound(str(p)).to_pitch_ac(time_step=0.01, pitch_floor=75, pitch_ceiling=500)
        f = pitch.selected_array["frequency"]; vals += list(f[f > 0])
    else:
        if vals:
            out[v] = {"f0_median": round(float(np.median(vals)), 1),
                      "f0_iqr": [round(float(np.percentile(vals, 25)), 1), round(float(np.percentile(vals, 75)), 1)],
                      "voiced_frames": len(vals)}
Path("/out/f0.json").write_text(json.dumps(out))
print(len(out))
