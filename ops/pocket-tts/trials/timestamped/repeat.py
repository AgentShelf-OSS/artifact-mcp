"""Warm, same-text repeats to distinguish fork overhead from noisy single runs."""
import gc
import json
import logging
import time
import statistics
from pathlib import Path
import torch
from pocket_tts import TTSModel as Baseline
from pocket_tts_timestamped import TTSModel, AudioChunk
logging.disable(logging.INFO)
torch.set_num_threads(1)
torch.set_num_interop_threads(1)
rows=[]
text='The library was quiet. Beyond the window, rain traced silver lines across the glass. She opened the book and began to read, slowly at first, then with growing confidence.'
for mode, cls in [('baseline-int8',Baseline),('fork-plain-int8',TTSModel),('fork-timestamped-int8',TTSModel)]:
 model=cls.load_model('english_2026-04',quantize=True)
 voice=model.get_state_for_audio_prompt('alba')
 method=model.generate_audio_with_timestamps_stream if mode=='fork-timestamped-int8' else model.generate_audio_stream
 list(method(voice,'Ready to read.',copy_state=True))
 for repeat in range(3):
  torch.manual_seed(1234)
  start=time.monotonic(); first=None; samples=0
  for e in method(voice,text,copy_state=True):
   audio=e.audio if isinstance(e,AudioChunk) else e if isinstance(e,torch.Tensor) else None
   if audio is not None:
    if first is None: first=time.monotonic()-start
    samples+=audio.numel()
  rows.append(dict(mode=mode,repeat=repeat,first_audio_seconds=first,generation_seconds=time.monotonic()-start,audio_seconds=samples/model.sample_rate))
 del voice,model
 gc.collect()
Path('/results/repeats.json').write_text(json.dumps(rows,indent=2))
for mode in dict.fromkeys(r['mode'] for r in rows):
 subset=[r for r in rows if r['mode']==mode]
 print(mode, 'median first',statistics.median(r['first_audio_seconds'] for r in subset),'median generation',statistics.median(r['generation_seconds'] for r in subset),flush=True)
