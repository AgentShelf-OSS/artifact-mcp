import json
from pathlib import Path

import numpy as np
import soundfile as sf
import torch
from pocket_tts_timestamped import TTSModel

ROOT = Path('/trial')
OUT = ROOT / 'audio'
OUT.mkdir(exist_ok=True)
text = 'Winston Smith, alone in the room, listened to the faint mechanical hum.'
torch.set_num_threads(1)
torch.set_num_interop_threads(1)
model = TTSModel.load_model('english_2026-04', quantize=True)
audio = model.generate_audio(model.get_state_for_audio_prompt('alba'), text, copy_state=True).detach().cpu().numpy()
audio = np.clip(audio, -1.0, 1.0)
target = OUT / 'alba_baseline.wav'
sf.write(target, audio, model.sample_rate, subtype='PCM_16')
(ROOT / 'alba-generation.json').write_text(json.dumps({
    'text': text,
    'model': 'pocket-tts-timestamped-65037e84-english_2026-04-int8',
    'voice': 'alba',
    'sample_rate': model.sample_rate,
    'frames': int(audio.shape[0]),
    'duration_seconds': round(float(audio.shape[0]) / model.sample_rate, 3),
    'bytes': target.stat().st_size,
}, indent=2) + '\n')
