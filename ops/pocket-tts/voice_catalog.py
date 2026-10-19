#!/usr/bin/env python3
"""Build the artifact-mcp Pocket voice catalog from the audition choices.

The catalog (POCKET_TTS_VOICE_CATALOG) controls which Pocket voices users see.
It always lists every voice that is exposed today: the built-in presets in
src/http/routes/speech.rs and the POCKET_TTS_EXTRA_VOICES entries. Audition
candidates appear only after Neil marks them Keep.

Inputs
  --metrics      voice-metrics.json from the donated-voice audition (gain, F0 gender estimate)
  --state-file   audition state value (JSON) ...or...
  --from-ct220   read it read-only from CT220 SQLite (pct exec 220) by artifact key
  --extra-voices POCKET_TTS_EXTRA_VOICES value; with --from-ct220 it is read from
                 the CT220 systemd drop-in when omitted
  --worker-manifest  current worker voices/manifest.json (optional)

Outputs (in --out)
  pocket-voices.json   catalog for artifact-mcp, format version 1
  worker-manifest.json the worker manifest with Keep labels applied and any
                       missing Keep voice appended (only with --worker-manifest)
  summary.json         counts and the list of kept, maybe, and skipped voices

Nothing is installed or restarted. Copy the outputs into place yourself.
"""
import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
SPEECH_RS = HERE.parent.parent / "src" / "http" / "routes" / "speech.rs"
ARTIFACT_KEY = "voices/pocket-donated-audition"
ORG = "homelab"
STATE_KEY = "audition"
DROPIN = "/etc/systemd/system/artifact-mcp.service.d/pocket-extra-voices.conf"
DONATED_EXTRAS = {"andrea", "darius", "hannah", "bedfordshire"}
ID_RE = re.compile(r"^[a-z0-9_]{1,40}$")
GENDERS = {"female", "male", "neutral", None}
SOURCES = {"pocket-preset", "kyutai-donation", "voice-zero", "custom"}
MAX_ENTRIES = 400


def fail(message):
    sys.exit("voice_catalog: " + message)


def builtin_voices(path):
    text = Path(path).read_text()
    block = text[text.index("const POCKET_VOICES"):]
    block = block[:block.index("];")]
    found = re.findall(r'\("pocket_([a-z0-9_]+)",\s*"(.+?) · Pocket",\s*(-?[0-9.]+)\)', block)
    if len(found) < 20:
        fail(f"parsed only {len(found)} built-in voices from {path}")
    return [(vid, label, float(gain)) for vid, label, gain in found]


def parse_extra(value):
    out = []
    for item in filter(None, (part.strip() for part in value.split(","))):
        parts = item.split(":")
        if len(parts) not in (2, 3):
            fail(f"bad POCKET_TTS_EXTRA_VOICES item {item!r}")
        out.append((parts[0], parts[1], float(parts[2]) if len(parts) == 3 and parts[2] else 0.0))
    return out


def pct(script):
    result = subprocess.run(["pct", "exec", "220", "--", "python3", "-c", script],
                            check=True, capture_output=True, text=True, timeout=60)
    return result.stdout


def state_from_ct220():
    script = (
        "import json,sqlite3\n"
        "c=sqlite3.connect('file:/var/lib/artifact-mcp/artifacts.db?mode=ro',uri=True)\n"
        f"a=c.execute('select id from artifacts where artifact_key=? and org=?',({ARTIFACT_KEY!r},{ORG!r})).fetchone()\n"
        "r=a and c.execute(\"select value,revision,updated_at from artifact_state where artifact_id=? and scope='org' and viewer='' and key=?\","
        f"(a[0],{STATE_KEY!r})).fetchone()\n"
        "print(json.dumps({'artifact':a and a[0],'value':r and json.loads(r[0]),'revision':r and r[1],'updated_at':r and r[2]}))\n")
    data = json.loads(pct(script))
    if not data["artifact"]:
        fail(f"no artifact with key {ARTIFACT_KEY} in org {ORG}")
    return data


def extra_from_ct220():
    script = (f"import re;t=open({DROPIN!r}).read()\n"
              "m=re.findall(r'^Environment=POCKET_TTS_EXTRA_VOICES=(.*)$',t,re.M)\nprint(m[-1] if m else '')\n")
    return pct(script).strip()


def clean_label(text):
    text = re.sub(r"[^\x20-\x7e]", "", str(text or "")).replace(",", "").replace(":", "")
    return re.sub(r"\s+", " ", text).strip()[:40].strip()


def default_label(source_id):
    base = source_id.removesuffix("_enhanced")
    if re.fullmatch(r"[0-9a-f]{4}", base):
        base = "Donor " + base
    base = base.replace("_", " ").replace("-", " ")
    label = clean_label(base)
    return label[:1].upper() + label[1:] if label else "Donated voice"


def short(value):
    value = clean_label(value)
    return value[:24] if value else None


def validate(catalog):
    voices = catalog["voices"]
    if len(voices) > MAX_ENTRIES:
        fail(f"catalog has {len(voices)} entries (max {MAX_ENTRIES})")
    seen = set()
    for v in voices:
        if set(v) != {"id", "label", "gainDb", "gender", "accent", "style", "source"}:
            fail(f"bad fields in {v}")
        if not ID_RE.fullmatch(v["id"]) or v["id"] in seen:
            fail(f"bad or duplicate id {v['id']!r}")
        seen.add(v["id"])
        label = v["label"]
        if not (1 <= len(label) <= 40) or any(not (32 <= ord(ch) <= 126) or ch in ",:" for ch in label):
            fail(f"bad label {label!r}")
        if not isinstance(v["gainDb"], (int, float)) or not -12 <= v["gainDb"] <= 12:
            fail(f"bad gainDb for {v['id']}")
        if v["gender"] not in GENDERS or v["source"] not in SOURCES:
            fail(f"bad gender/source for {v['id']}")
        for key in ("accent", "style"):
            if v[key] is not None and (not isinstance(v[key], str) or not 1 <= len(v[key]) <= 24):
                fail(f"bad {key} for {v['id']}")


def number(value):
    return int(value) if float(value).is_integer() else value


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--metrics", required=True)
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--state-file")
    src.add_argument("--from-ct220", action="store_true")
    ap.add_argument("--extra-voices")
    ap.add_argument("--worker-manifest")
    ap.add_argument("--speech-rs", default=str(SPEECH_RS))
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    metrics = json.loads(Path(args.metrics).read_text())
    existing, candidates = metrics["existing"], metrics["candidates"]
    if args.from_ct220:
        snapshot = state_from_ct220()
        state = snapshot["value"] or {"d": {}}
        provenance = {k: snapshot[k] for k in ("artifact", "revision", "updated_at")}
        extra_value = args.extra_voices if args.extra_voices is not None else extra_from_ct220()
    else:
        state = json.loads(Path(args.state_file).read_text())
        provenance = {"state_file": args.state_file}
        extra_value = args.extra_voices
    if extra_value is None:
        fail("--extra-voices is required with --state-file")
    choices = state.get("d", {}) if isinstance(state, dict) else {}

    voices = []
    def gender_of(vid):
        return (existing.get(vid) or candidates.get(vid) or {}).get("gender_estimate")
    for vid, label, gain in builtin_voices(args.speech_rs):
        voices.append({"id": vid, "label": label, "gainDb": number(gain), "gender": gender_of(vid),
                       "accent": None, "style": None, "source": "pocket-preset"})
    builtin_ids = {v["id"] for v in voices}
    for vid, label, gain in parse_extra(extra_value):
        if vid in builtin_ids:
            fail(f"extra voice {vid} duplicates a built-in")
        voices.append({"id": vid, "label": clean_label(label), "gainDb": number(gain), "gender": gender_of(vid),
                       "accent": None, "style": None,
                       "source": "kyutai-donation" if vid in DONATED_EXTRAS else "pocket-preset"})
    exposed = {v["id"] for v in voices}

    kept, maybe, skipped = [], [], []
    for vid in metrics["selected"]:
        choice = choices.get(vid) or {}
        decision = choice.get("c")
        (kept if decision == "keep" else maybe if decision == "maybe" else skipped if decision == "skip" else []).append(vid)
        if decision != "keep" or vid in exposed:
            continue
        cand = candidates[vid]
        gender = choice.get("g") if choice.get("g") in ("female", "male", "neutral") else cand.get("gender_estimate")
        accent = short(choice.get("a"))
        voices.append({"id": vid, "label": clean_label(choice.get("l")) or default_label(cand["source_id"]),
                       "gainDb": number(cand["gain_db"] if cand.get("gain_db") is not None else 0),
                       "gender": gender, "accent": None if accent == "Other/unsure" else accent,
                       "style": short(choice.get("s")), "source": "kyutai-donation"})

    catalog = {"version": 1, "voices": voices}
    validate(catalog)
    out = Path(args.out); out.mkdir(parents=True, exist_ok=True)
    (out / "pocket-voices.json").write_text(json.dumps(catalog, indent=1) + "\n")

    if args.worker_manifest:
        manifest = json.loads(Path(args.worker_manifest).read_text())
        by_voice = {r["voice"]: r for r in manifest["results"]}
        labels = {v["id"]: v["label"] for v in voices}
        missing = [vid for vid in kept if vid not in by_voice]
        if missing:
            fail("kept voices missing from the worker manifest: " + ", ".join(missing))
        for vid in kept:
            by_voice[vid]["label"] = labels[vid]
        (out / "worker-manifest.json").write_text(json.dumps(manifest, indent=1) + "\n")

    summary = {"provenance": provenance, "catalog_entries": len(voices), "built_in": len(builtin_ids),
               "extra": len(exposed) - len(builtin_ids), "kept": kept, "maybe": maybe, "skipped": skipped,
               "undecided": len(metrics["selected"]) - len(kept) - len(maybe) - len(skipped)}
    (out / "summary.json").write_text(json.dumps(summary, indent=1) + "\n")
    print(json.dumps({k: (len(v) if isinstance(v, list) else v) for k, v in summary.items()}))


if __name__ == "__main__":
    main()
