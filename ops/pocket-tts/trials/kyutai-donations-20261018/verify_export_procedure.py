import math, hashlib, sys
from pathlib import Path
import numpy as np, soundfile as sf, torch
from safetensors.torch import load_file
from pocket_tts_timestamped import TTSModel, export_model_state
torch.set_num_threads(int(sys.argv[1])); torch.set_num_interop_threads(1)
ref = load_file("/out/ref_andrea.safetensors")
src = Path("/work/source/Andrea_enhanced.wav")
m = TTSModel.load_model("english_2026-04", quantize=True)
def cmp(name, st):
    export_model_state(st, f"/tmp/{name}.safetensors"); t = load_file(f"/tmp/{name}.safetensors")
    d = [float((t[k].float()-ref[k].float()).abs().mean()) for k in ref if t[k].dtype.is_floating_point]
    print(sys.argv[1], name, hashlib.sha256(Path(f"/tmp/{name}.safetensors").read_bytes()).hexdigest()[:12], "mean_abs_diff", round(float(np.mean(d)), 6), flush=True)
data, sr = sf.read(src, dtype="float32")
data64, _ = sf.read(src, dtype="float64")
rms = 20*math.log10(float(np.sqrt(np.mean(data64**2)))); peak = float(np.max(np.abs(data64)))
gd = min(-20 - rms, 20*math.log10(0.9/peak))
for name, x in (("exact", data*10**(gd/20)), ("rounded", data*10**(round(gd,2)/20)), ("f64", data64*10**(gd/20))):
    sf.write("/tmp/a.wav", x, sr, subtype="PCM_16"); cmp(name, m.get_state_for_audio_prompt(Path("/tmp/a.wav"), truncate=True))
