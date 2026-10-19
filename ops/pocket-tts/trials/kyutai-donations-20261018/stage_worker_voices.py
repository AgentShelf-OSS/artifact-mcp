"""Copy audition candidates into the worker voice directory and append manifest entries.

Usage: stage_worker_voices.py <export-manifest.json> <voice-metrics.json> <states-dir> <voices-dir>

Idempotent. Verifies each state's SHA-256 before copying. Writes manifest.json
atomically and keeps manifest.json.pre-audition-<timestamp> as a rollback copy.
Loading a voice in the worker does not expose it: artifact-mcp's catalog does.
A VCTK report ("source": "vctk") stages its suggested names as labels and records the
speaker-info gender and accent; the export rows carry the CC BY 4.0 attribution.
"""
import hashlib, json, shutil, sys, time
from pathlib import Path

export = {r["voice"]: r for r in json.loads(Path(sys.argv[1]).read_text())["results"]}
metrics = json.loads(Path(sys.argv[2]).read_text())
vctk = metrics.get("source") == "vctk"
states, voices = Path(sys.argv[3]), Path(sys.argv[4])
path = voices / "manifest.json"
manifest = json.loads(path.read_text())
have = {r["voice"] for r in manifest["results"]}
added = 0
for vid in metrics["selected"]:
    if vid in have:
        continue
    row, cand = export[vid], metrics["candidates"][vid]
    src = states / (vid + ".safetensors")
    if hashlib.sha256(src.read_bytes()).hexdigest() != row["embedding_sha256"]:
        sys.exit(f"checksum mismatch for {vid}")
    dest = voices / src.name
    shutil.copyfile(src, dest)
    dest.chmod(0o644)
    label = cand["label"] if vctk else row["source_id"].removesuffix("_enhanced").replace("_", " ")
    audition = {"rank": cand["rank"], "gain_db": cand["gain_db"], "gender_estimate": cand["gender_estimate"]}
    if vctk:
        audition.update({"accent": cand["accent"], "region": cand["region"], "f0_check": cand["f0_check"]})
    entry = {"voice": vid, "label": label[:40], **{k: v for k, v in row.items() if k != "voice"},
             "source": "vctk" if vctk else "kyutai-donation", "audition": audition}
    entry.pop("export_seconds", None)
    manifest["results"].append(entry)
    added += 1
if added:
    shutil.copyfile(path, voices / f"manifest.json.pre-{'vctk' if vctk else 'audition'}-{time.strftime('%Y%m%d-%H%M%S')}")
    tmp = voices / ".manifest.json.tmp"
    tmp.write_text(json.dumps(manifest, indent=2) + "\n")
    tmp.chmod(0o644)
    tmp.replace(path)
print(json.dumps({"added": added, "total": len(manifest["results"])}))
