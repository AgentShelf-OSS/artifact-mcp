import base64
import html
from pathlib import Path

root = Path(__file__).parent
source = root / 'source'
preview = root / 'preview'
preview.mkdir(exist_ok=True)
meta = {}
for line in (root / 'sources.tsv').read_text().splitlines()[1:]:
    fields = line.split('\\t')
    if len(fields) >= 6:
        meta[fields[0]] = {'label': fields[1], 'accent': fields[2], 'url': fields[3]}

cards = []
for item in sorted(meta):
    mp3 = preview / (item + '.mp3')
    if not mp3.exists():
        continue
    encoded = base64.b64encode(mp3.read_bytes()).decode('ascii')
    data = meta[item]
    cards.append('<article class="card"><div class="eyebrow">SOURCE RECORDING</div>'
        f'<h2>{html.escape(data["label"])}</h2><p>{html.escape(data["accent"])}</p>'
        f'<audio controls preload="none" src="data:audio/mpeg;base64,{encoded}"></audio>'
        f'<a href="{html.escape(data["url"])}">Pinned Voice-Zero source</a></article>')

alba = root / 'audio' / 'alba_baseline.wav'
alba_card = ''
if alba.exists():
    encoded = base64.b64encode(alba.read_bytes()).decode('ascii')
    alba_card = ('<article class="card control"><div class="eyebrow">GENERATED BASELINE</div>'
        '<h2>Alba</h2><p>Current worker control sample.</p>'
        f'<audio controls preload="none" src="data:audio/wav;base64,{encoded}"></audio></article>')

page = '''<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Voice-Zero Pocket TTS audition preparation</title>
<style>*{box-sizing:border-box}body{margin:0;background:#101114;color:#f4f1ea;font:16px/1.5 system-ui,sans-serif}main{max-width:980px;margin:auto;padding:48px 22px}h1{font-size:clamp(2rem,5vw,4rem);line-height:1.02;margin:0 0 14px}.intro{color:#bdb9b0;max-width:680px}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(260px,1fr));gap:14px;margin-top:30px}.card{padding:20px;border:1px solid #34363c;border-radius:16px;background:#181a1f}.control{border-color:#b7a36a}.eyebrow{font-size:11px;letter-spacing:.16em;color:#c5aa64;font-weight:700}h2{margin:8px 0 2px;font-size:1.25rem}p{color:#bdb9b0;margin:0 0 16px}audio{width:100%;margin:8px 0 14px}a{color:#d9c58b;font-size:13px}</style>
<main><h1>Voice-Zero audition</h1><p class="intro">Reference recordings prepared for a future isolated Pocket TTS test. The six cards below are original Voice-Zero source recordings, not generated narration. The Alba card is a generated control sample from the current worker.</p><section class="grid">''' + alba_card + ''.join(cards) + '''</section><p style="margin-top:24px">Source collection: <a href="https://github.com/OwenTyme/voice-zero">Voice-Zero</a>, listed as CC0. Accent descriptions are approximate. These recordings do not predict the quality of generated Pocket narration.</p></main><script>document.addEventListener('play',function(event){if(event.target.tagName==='AUDIO')document.querySelectorAll('audio').forEach(function(audio){if(audio!==event.target)audio.pause();});},true);</script>'''
(root / 'voice-zero-source-preview.html').write_text(page)
