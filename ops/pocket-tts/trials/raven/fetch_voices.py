"""Fetch only the two official preset references, pinned to one HF revision."""
import hashlib
import json
from pathlib import Path
import urllib.request

root=Path('voices');root.mkdir(exist_ok=True)
repo='kyutai/tts-voices'
revision='323332d33f997de8394f24a193e1a76df720e01a' # revision used for the listening trial
manifest={'repository':repo,'revision':revision,'voices':{}}
for name,path in {'alba':'alba-mackenna/casual.wav','marius':'voice-donations/Selfie.wav'}.items():
 url=f'https://huggingface.co/{repo}/resolve/{revision}/{path}'
 with urllib.request.urlopen(url,timeout=60) as r:payload=r.read(20*1024*1024+1)
 assert len(payload)<=20*1024*1024
 (root/(name+'.wav')).write_bytes(payload)
 manifest['voices'][name]={'url':url,'bytes':len(payload),'sha256':hashlib.sha256(payload).hexdigest()}
(root/'manifest.json').write_text(json.dumps(manifest,indent=2))
print(json.dumps(manifest))
