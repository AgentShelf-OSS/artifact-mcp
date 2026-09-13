"""Persistent CPU-engine comparison. Writes measured audio; no service endpoints."""
import ctypes as C
import json
import os
from pathlib import Path
import resource
import time
import numpy as np
import soundfile as sf

OUT=Path('/results')
ENGINE=os.environ.get('ENGINE','raven')
TEXTS={
 'prose':'The library was quiet. Beyond the window, rain traced silver lines across the glass. She opened the book and began to read, slowly at first, then with growing confidence.',
 'mixed':'The report lists three priorities: reduce waiting time, improve navigation, and preserve reading progress. Revenue rose 12.5%, reaching $1,240. Dr. Evans reviewed the API results on September 12, 2026.',
 'long':' '.join(['At the edge of the garden, a narrow path led towards the old stone bridge. The river moved quietly beneath it, carrying fallen leaves towards the distant hills.']*5)
}
report={'engine':ENGINE,'temperature':.3,'cpu_quota':2,'memory_limit_gib':3,'runs':[]}
start=time.monotonic()
if ENGINE=='raven':
 lib=C.CDLL('/raven/libpocket_tts.so'); P=C.POINTER(C.c_float)
 lib.ptt_create.argtypes=[C.c_char_p]*4+[C.c_float,C.c_int,C.c_int];lib.ptt_create.restype=C.c_void_p
 lib.ptt_destroy.argtypes=[C.c_void_p];lib.ptt_destroy.restype=None
 lib.ptt_set_soften_commas.argtypes=[C.c_void_p,C.c_int];lib.ptt_set_soften_commas.restype=None
 lib.ptt_stream_start.argtypes=[C.c_void_p,C.c_char_p,C.c_char_p];lib.ptt_stream_start.restype=C.c_void_p
 lib.ptt_stream_read.argtypes=[C.c_void_p,C.POINTER(P),C.POINTER(C.c_int)];lib.ptt_stream_read.restype=C.c_int
 for name in ['ptt_stream_stop','ptt_stream_end']:
  getattr(lib,name).argtypes=[C.c_void_p];getattr(lib,name).restype=None
 lib.ptt_free_audio.argtypes=[P];lib.ptt_free_audio.restype=None
 model=lib.ptt_create(b'/raven/models',b'/voices',b'/raven/models/tokenizer.model',b'int8',.3,1,2)
 if not model:raise RuntimeError('RAVEN initialization failed')
 lib.ptt_set_soften_commas(model,0) # preserve authored clause punctuation, as Pocket does
 report['model_load_seconds']=time.monotonic()-start
 def stream(voice,text):
  ctx=lib.ptt_stream_start(model,text.encode(),(voice+'.wav').encode())
  if not ctx:raise RuntimeError('RAVEN could not start stream')
  try:
   while True:
    ptr=P();n=C.c_int()
    if lib.ptt_stream_read(ctx,C.byref(ptr),C.byref(n))!=1:break
    try:yield np.ctypeslib.as_array(ptr,shape=(n.value,)).copy()
    finally:lib.ptt_free_audio(ptr)
  finally:lib.ptt_stream_stop(ctx);lib.ptt_stream_end(ctx)
else:
 import logging
 import torch
 from pocket_tts_timestamped import TTSModel,AudioChunk
 logging.disable(logging.INFO);torch.set_num_threads(1);torch.set_num_interop_threads(1)
 model=TTSModel.load_model('english_2026-04',quantize=True)
 states={v:model.get_state_for_audio_prompt(v) for v in ['alba','marius']}
 report['model_and_presets_load_seconds']=time.monotonic()-start
 def stream(voice,text):
  iterator=model.generate_audio_with_timestamps_stream(states[voice],text,copy_state=True)
  try:
   for event in iterator:
    if isinstance(event,AudioChunk):yield event.audio.cpu().numpy()
  finally:iterator.close()

def generate(voice,case,repeat,cold=False):
 start=time.monotonic();first=None;parts=[]
 iterator=stream(voice,TEXTS[case])
 try:
  for samples in iterator:
   if not len(samples):continue
   if first is None:first=time.monotonic()-start
   parts.append(samples)
 finally:iterator.close()
 elapsed=time.monotonic()-start
 if not parts:raise RuntimeError('Empty generation')
 samples=np.concatenate(parts)
 assert np.isfinite(samples).all()
 duration=len(samples)/24000
 name=f'{ENGINE}-{voice}-{case}-{repeat}'
 if repeat==0:sf.write(OUT/(name+'.flac'),samples,24000)
 row={'name':name,'voice':voice,'case':case,'repeat':repeat,'cold_voice_cache':cold,'first_audio_seconds':first,'generation_seconds':elapsed,'audio_seconds':duration,'realtime_multiple':duration/elapsed,'peak':float(np.max(np.abs(samples))),'rms':float(np.sqrt(np.mean(samples**2))),'peak_rss_mb':resource.getrusage(resource.RUSAGE_SELF).ru_maxrss/1024}
 report['runs'].append(row);(OUT/(ENGINE+'-results.json')).write_text(json.dumps(report,indent=2));print(json.dumps(row),flush=True)
try:
 for voice in ['alba','marius']:
  generate(voice,'prose',-1,cold=ENGINE=='raven' and not all((Path('/voices/.cache')/(voice+ext)).is_file() for ext in ['.emb','.kv']))
  for repeat in range(3):generate(voice,'prose',repeat)
 for repeat in range(3):generate('alba','mixed',repeat)
 generate('alba','long',0)
 iterator=stream('alba',TEXTS['long']*4);next(iterator);start=time.monotonic();iterator.close();report['cancel_seconds']=time.monotonic()-start
 generate('alba','prose',99)
 (OUT/(ENGINE+'-results.json')).write_text(json.dumps(report,indent=2))
finally:
 if ENGINE=='raven':lib.ptt_destroy(model)
