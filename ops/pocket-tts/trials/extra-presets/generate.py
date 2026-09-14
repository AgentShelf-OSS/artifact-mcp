import hashlib,json,time,urllib.request
from pathlib import Path
import numpy as np
import soundfile as sf
import torch
from pocket_tts_timestamped import TTSModel
ROOT=Path('/trial');OUT=ROOT/'audio';OUT.mkdir(exist_ok=True)
TEXT='The garden was quiet in the early morning. She opened the book and began to read. Beyond the window, a distant bell marked the beginning of another day. There was no reason to hurry.'
torch.set_num_threads(1);torch.set_num_interop_threads(1)
model=TTSModel.load_model('english_2026-04',quantize=True)
results=[]
for voice in ['alba','estelle','giovanni','juergen','lola','rafael']:
    started=time.monotonic()
    reference=ROOT/(voice+'.safetensors')
    url='https://huggingface.co/kyutai/pocket-tts-without-voice-cloning/resolve/e81d79e8194ad4c7ce879c87a4258ef20cbf2487/languages/english/embeddings/'+voice+'.safetensors'
    urllib.request.urlretrieve(url,reference)
    state=model.get_state_for_audio_prompt(reference)
    audio=model.generate_audio(state,TEXT,copy_state=True).detach().cpu().numpy().reshape(-1)
    assert np.isfinite(audio).all() and len(audio)>24000
    peak=float(np.max(np.abs(audio))); clipped=float(np.mean(np.abs(audio)>1))
    assert clipped<0.001, 'Excessive clipping'
    target=OUT/(voice+'.wav');sf.write(target,np.clip(audio,-1,1),model.sample_rate,subtype='PCM_16')
    result={'voice':voice,'duration_seconds':round(len(audio)/model.sample_rate,3),'sample_rate':model.sample_rate,'peak':peak,'clipped_fraction':clipped,'generation_seconds':round(time.monotonic()-started,3),'sha256':hashlib.sha256(target.read_bytes()).hexdigest()}
    results.append(result);print(json.dumps(result),flush=True)
(ROOT/'results.json').write_text(json.dumps({'text':TEXT,'model':'pocket-tts-timestamped-65037e84-english_2026-04-int8','voice_revision':'e81d79e8194ad4c7ce879c87a4258ef20cbf2487','results':results},indent=2)+'\n')
