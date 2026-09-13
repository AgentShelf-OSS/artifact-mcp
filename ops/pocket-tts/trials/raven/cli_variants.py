"""CLI startup and listening checks; times INCLUDE process and model loading."""
import json, os, subprocess, time
from pathlib import Path
import numpy as np
import soundfile as sf
text='The library was quiet. Beyond the window, rain traced silver lines across the glass. She opened the book and began to read, slowly at first, then with growing confidence.'
rows=[]
for name,flags in [('default',[]),('low-latency',['--low-latency']),('low-latency-trim',['--low-latency','--trim-leading'])]:
 start=time.monotonic();parts=[];first=None
 with open('/results/cli-'+name+'.log','wb') as log:
  process=subprocess.Popen(['/raven/pocket-tts','--models-dir','/raven/models','--voices-dir','/voices','--tokenizer','/raven/models/tokenizer.model','--threads','2','--temperature','0.3','--keep-commas','--stdout',*flags,text,'alba.wav'],stdout=subprocess.PIPE,stderr=log)
  try:
   while True:
    part=os.read(process.stdout.fileno(),65536)
    if not part:break
    if first is None:first=time.monotonic()-start
    parts.append(part)
   assert process.wait(timeout=30)==0
  finally:
   if process.poll() is None:process.kill();process.wait()
 elapsed=time.monotonic()-start
 samples=np.frombuffer(b''.join(parts),dtype='<f4');assert len(samples)>0 and np.isfinite(samples).all()
 sf.write('/results/raven-cli-'+name+'.flac',samples,24000)
 rows.append({'variant':name,'first_audio_including_load_seconds':first,'total_including_load_seconds':elapsed,'audio_seconds':len(samples)/24000})
Path('/results/cli-results.json').write_text(json.dumps(rows,indent=2));print(json.dumps(rows))
