import json
from pathlib import Path

import numpy as np
import soundfile as sf
import torch
from pocket_tts_timestamped import TTSModel

ROOT = Path('/trial')
SOURCE = ROOT / 'source'
OUT = ROOT / 'audio'
TEXT = 'Winston Smith, alone in the room, listened to the faint mechanical hum.'

torch.set_num_threads(1)
torch.set_num_interop_threads(1)
model = TTSModel.load_model('english_2026-04', quantize=True)
results = []
for source in sorted(SOURCE.glob('*.flac')):
    voice = model.get_state_for_audio_prompt(str(source))
    audio = model.generate_audio(voice, TEXT, copy_state=True).detach().cpu().numpy()
    audio = np.clip(audio, -1.0, 1.0)
    target = OUT / (source.stem + '.wav')
    sf.write(target, audio, model.sample_rate, subtype='PCM_16')
    results.append({
        'id': source.stem,
        'file': target.name,
        'sample_rate': model.sample_rate,
        'frames': int(audio.shape[0]),
        'duration_seconds': round(float(audio.shape[0]) / model.sample_rate, 3),
        'bytes': target.stat().st_size,
    })

(ROOT / 'generation.json').write_text(json.dumps({
    'text': TEXT,
    'model': 'english_2026-04',
    'quantized': True,
    'results': results,
}, indent=2) + '\n')
