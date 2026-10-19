"""Build the Pocket voice audition bundle.

Usage: build_audition.py <voice-metrics.json> <audio-dir> <out-dir> [template.html]
The template defaults to audition.html next to this script. Candidates with "label",
"accent", "region", or "age" (the VCTK audition) pass them to the page as lbl/acc/reg/age.
Voice-Zero credit fields (narrator, narrator_url, reading, reading_url, source_file,
readme_match) pass as nar/nurl/work/wurl/file/match.
Writes out-dir/index.html and out-dir/audio-NN.js (base64 MP3 data URIs),
plus out-dir/bundle.json ({path: content}) for publish_bundle.
"""
import base64, html, json, subprocess, sys
from pathlib import Path

report = json.loads(Path(sys.argv[1]).read_text())
AUDIO = Path(sys.argv[2]); OUT = Path(sys.argv[3]); OUT.mkdir(parents=True, exist_ok=True)
PER_CHUNK = 8
sel = [report["candidates"][v] for v in report["selected"]]

def mp3(path):
    out = subprocess.run(["ffmpeg", "-v", "error", "-i", str(path), "-ac", "1", "-ar", "24000", "-c:a", "libmp3lame",
                          "-b:a", "32k", "-f", "mp3", "-"], check=True, capture_output=True).stdout
    return "data:audio/mpeg;base64," + base64.b64encode(out).decode()

def wpm_tag(w):
    return "slow" if w < 145 else "brisk" if w > 185 else "steady"

voices, chunks = [], []
for i, r in enumerate(sel):
    c = i // PER_CHUNK
    if c == len(chunks):
        chunks.append({})
    chunks[c][r["voice"]] = [mp3(AUDIO / f"{r['voice']}_neutral.wav"), mp3(AUDIO / f"{r['voice']}_story.wav")]
    v = {"id": r["voice"], "src": r["source_id"].removesuffix("_enhanced"), "c": c, "rank": r["rank"],
         "g": r["gender_estimate"], "f0": r["f0_median"], "wpm": round(r["wpm"]), "pace": wpm_tag(r["wpm"]),
         "wer": round(r["wer_mean"] * 100, 1), "gain": r["gain_db"]}
    for key, short in (("label", "lbl"), ("accent", "acc"), ("region", "reg"), ("age", "age"),
                       ("narrator", "nar"), ("narrator_url", "nurl"), ("reading", "work"),
                       ("reading_url", "wurl"), ("source_file", "file"), ("readme_match", "match")):
        if r.get(key) is not None:
            v[short] = r[key]
    voices.append(v)

files = {}
for c, data in enumerate(chunks):
    files[f"audio-{c:02d}.js"] = "window.__aud&&window.__aud(%d,%s);\n" % (c, json.dumps(data, separators=(",", ":")))

S = report["summary"]
tpl = Path(sys.argv[4] if len(sys.argv) > 4 else Path(__file__).with_name("audition.html")).read_text()
page = (tpl.replace("__VOICES__", json.dumps(voices, separators=(",", ":")))
           .replace("__NCAND__", str(S["candidates"])).replace("__NSEL__", str(S["selected"]))
           .replace("__SENT_N__", html.escape(report["sentences"]["neutral"]))
           .replace("__SENT_S__", html.escape(report["sentences"]["story"])))
files["index.html"] = page
for name, body in files.items():
    (OUT / name).write_text(body)
(OUT / "bundle.json").write_text(json.dumps(files))
total = sum(len(b.encode()) for b in files.values())
print(json.dumps({"files": len(files), "bytes": total, "largest": max(len(b) for b in files.values())}))
