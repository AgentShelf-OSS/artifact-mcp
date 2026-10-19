"""Synthesize the two audition sentences per voice with the production (ungated, int8) model."""
import json, os, sys, time
from pathlib import Path
os.environ["KPOCKET_TTS_ERROR_WITHOUT_EOS"] = "1"
import numpy as np, soundfile as sf, torch
from pocket_tts_timestamped import TTSModel
torch.set_num_threads(1); torch.set_num_interop_threads(1)
SENTENCES = {
    "neutral": "Please check the schedule before Thursday. The meeting has moved to room twelve, near the north entrance.",
    "story": "Long ago, in a village by the sea, an old woman kept a lantern burning every night, waiting for a ship that never came home.",
}
shard, count = int(sys.argv[1]), int(sys.argv[2])
jobs = json.load(open("/work/synth-jobs.json"))[shard::count]
OUT = Path("/out/audio"); OUT.mkdir(exist_ok=True)
model = TTSModel.load_model("english_2026-04", quantize=True)
assert not model.has_voice_cloning
log = open(f"/out/synth-{shard}.jsonl", "a")
for job in jobs:
    v = job["voice"]
    if all((OUT / f"{v}_{k}.wav").exists() for k in SENTENCES):
        continue
    rec = {"voice": v}
    try:
        state = model.get_state_for_audio_prompt(job["prompt"] if job["kind"] == "preset" else Path(job["prompt"]))
        for k, text in SENTENCES.items():
            torch.manual_seed(20261018)
            t0 = time.monotonic()
            a = model.generate_audio(state, text, copy_state=True).detach().cpu().numpy().reshape(-1)
            sf.write(OUT / f"{v}_{k}.wav", np.clip(a, -1, 1), model.sample_rate, subtype="PCM_16")
            rec[k] = {"seconds": round(len(a)/model.sample_rate, 3), "gen_seconds": round(time.monotonic()-t0, 2)}
    except Exception as e:
        rec["error"] = f"{type(e).__name__}: {e}"[:300]
    log.write(json.dumps(rec) + "\n"); log.flush(); print(json.dumps(rec), flush=True)
