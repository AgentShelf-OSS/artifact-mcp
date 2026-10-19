#!/usr/bin/env python3
"""Build the artifact-mcp Pocket voice catalog from the audition choices.

The catalog (POCKET_TTS_VOICE_CATALOG) controls which Pocket voices users see.
It always lists every voice that is exposed today: the built-in presets in
src/http/routes/speech.rs and the POCKET_TTS_EXTRA_VOICES entries. Audition
candidates appear only after Neil marks them Keep. Several auditions merge into
one catalog, in this order: built-ins, extras, then each audition's kept voices.

Auditions (repeat --audition; the metrics file's "source" field picks the kind)
  kyutai  voice-metrics.json without "source"; artifact key voices/pocket-donated-audition;
          catalog source "kyutai-donation"; gender from the F0 estimate
  vctk    voice-metrics.json with "source": "vctk"; artifact key voices/pocket-vctk-audition;
          catalog source "custom" (the worker manifest records "VCTK (CC BY 4.0)");
          gender and accent from VCTK speaker-info, name from the audition
  --metrics PATH is the same as --audition PATH (kept for the original command line).

Choices
  --state-file [KEY=]PATH  audition state value (JSON) for the audition with artifact key
                 KEY (default: the first audition). Repeatable.
  --from-ct220   read every audition's state read-only from CT220 SQLite (pct exec 220).
                 An audition with no artifact yet counts as no choices.
  With neither, no audition has choices (baseline catalog).

Other inputs
  --extra-voices POCKET_TTS_EXTRA_VOICES value; with --from-ct220 it is read from
                 the CT220 systemd drop-in when omitted
  --label-overrides  {voice id: label} applied last, so regenerating keeps Neil's
                 renames (default catalog/label-overrides.json)
  --worker-manifest  current worker voices/manifest.json (optional)

Outputs (in --out)
  pocket-voices.json   catalog for artifact-mcp, format version 1
  worker-manifest.json the worker manifest with Keep labels applied (only with
                       --worker-manifest)
  summary.json         counts and the kept, maybe, and skipped voices per audition

Nothing is installed or restarted. Copy the outputs into place yourself.
"""
import argparse
import json
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

HERE = Path(__file__).resolve().parent
SPEECH_RS = HERE.parent.parent / "src" / "http" / "routes" / "speech.rs"
AUDITIONS = {
    "kyutai": {"key": "voices/pocket-donated-audition", "source": "kyutai-donation"},
    "vctk": {"key": "voices/pocket-vctk-audition", "source": "custom"},
}
LABEL_OVERRIDES = HERE / "catalog" / "label-overrides.json"
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


def state_from_ct220(artifact_key):
    script = (
        "import json,sqlite3\n"
        "c=sqlite3.connect('file:/var/lib/artifact-mcp/artifacts.db?mode=ro',uri=True)\n"
        f"a=c.execute('select id from artifacts where artifact_key=? and org=?',({artifact_key!r},{ORG!r})).fetchone()\n"
        "r=a and c.execute(\"select value,revision,updated_at from artifact_state where artifact_id=? and scope='org' and viewer='' and key=?\","
        f"(a[0],{STATE_KEY!r})).fetchone()\n"
        "print(json.dumps({'artifact':a and a[0],'value':r and json.loads(r[0]),'revision':r and r[1],'updated_at':r and r[2]}))\n")
    return json.loads(pct(script))


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


def load_overrides(path):
    if not path or not Path(path).exists():
        return {}
    data = json.loads(Path(path).read_text())
    if not isinstance(data, dict):
        fail(f"{path}: expected an object of voice id to label")
    out = {}
    for vid, label in data.items():
        cleaned = clean_label(label)
        if not ID_RE.fullmatch(vid) or not cleaned:
            fail(f"{path}: bad override {vid!r}: {label!r}")
        out[vid] = cleaned
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--audition", action="append", default=[], metavar="METRICS")
    ap.add_argument("--metrics", action="append", default=[], help="same as --audition")
    src = ap.add_mutually_exclusive_group()
    src.add_argument("--state-file", action="append", default=[], metavar="[KEY=]PATH")
    src.add_argument("--from-ct220", action="store_true")
    ap.add_argument("--extra-voices")
    ap.add_argument("--label-overrides", default=str(LABEL_OVERRIDES))
    ap.add_argument("--worker-manifest")
    ap.add_argument("--speech-rs", default=str(SPEECH_RS))
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    auditions = []
    for path in args.metrics + args.audition:
        metrics = json.loads(Path(path).read_text())
        kind = metrics.get("source", "kyutai")
        if kind not in AUDITIONS:
            fail(f"{path}: unknown audition source {kind!r}")
        auditions.append({"kind": kind, "path": path, "metrics": metrics, **AUDITIONS[kind]})
    if not auditions:
        fail("give at least one --audition (or --metrics)")
    keys = [a["key"] for a in auditions]
    if len(set(keys)) != len(keys):
        fail("two auditions share an artifact key")

    state_files = {}
    for item in args.state_file:
        key, sep, path = item.rpartition("=")
        state_files[key if sep else auditions[0]["key"]] = path
    unknown = set(state_files) - set(keys)
    if unknown:
        fail("state file for an unknown audition: " + ", ".join(sorted(unknown)))
    for a in auditions:
        if args.from_ct220:
            snapshot = state_from_ct220(a["key"])
            a["state"] = (snapshot["value"] or {"d": {}}) if snapshot["artifact"] else {"d": {}}
            a["provenance"] = {k: snapshot[k] for k in ("artifact", "revision", "updated_at")}
        elif a["key"] in state_files:
            a["state"] = json.loads(Path(state_files[a["key"]]).read_text())
            a["provenance"] = {"state_file": state_files[a["key"]]}
        else:
            a["state"], a["provenance"] = {"d": {}}, {"state_file": None}
    if args.from_ct220:
        extra_value = args.extra_voices if args.extra_voices is not None else extra_from_ct220()
    else:
        extra_value = args.extra_voices
    if extra_value is None:
        fail("--extra-voices is required without --from-ct220")

    existing = {}
    for a in auditions:
        for vid, row in a["metrics"].get("existing", {}).items():
            existing.setdefault(vid, row)

    voices = []
    def gender_of(vid):
        if vid in existing:
            return existing[vid].get("gender_estimate")
        for a in auditions:
            if vid in a["metrics"]["candidates"]:
                return a["metrics"]["candidates"][vid].get("gender_estimate")
        return None
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
    fixed = len(voices)

    per_audition = {}
    for a in auditions:
        metrics, choices = a["metrics"], a["state"].get("d", {}) if isinstance(a["state"], dict) else {}
        candidates = metrics["candidates"]
        kept, maybe, skipped = [], [], []
        for vid in metrics["selected"]:
            choice = choices.get(vid) or {}
            decision = choice.get("c")
            (kept if decision == "keep" else maybe if decision == "maybe" else skipped if decision == "skip" else []).append(vid)
            if decision != "keep" or vid in exposed:
                continue
            cand = candidates[vid]
            gender = choice.get("g") if choice.get("g") in ("female", "male", "neutral") else cand.get("gender_estimate")
            accent = short(choice.get("a")) if choice.get("a") else short(cand.get("accent"))
            label = clean_label(choice.get("l")) or clean_label(cand.get("label")) or default_label(cand["source_id"])
            voices.append({"id": vid, "label": label,
                           "gainDb": number(cand["gain_db"] if cand.get("gain_db") is not None else 0),
                           "gender": gender, "accent": None if accent == "Other/unsure" else accent,
                           "style": short(choice.get("s")), "source": a["source"]})
            exposed.add(vid)
        per_audition[a["key"]] = {"kind": a["kind"], "provenance": a["provenance"], "kept": kept, "maybe": maybe,
                                  "skipped": skipped,
                                  "undecided": len(metrics["selected"]) - len(kept) - len(maybe) - len(skipped)}

    overrides = load_overrides(args.label_overrides)
    applied = []
    for v in voices:
        if v["id"] in overrides and v["label"] != overrides[v["id"]]:
            v["label"] = overrides[v["id"]]
            applied.append(v["id"])

    catalog = {"version": 1, "voices": voices}
    validate(catalog)
    out = Path(args.out); out.mkdir(parents=True, exist_ok=True)
    (out / "pocket-voices.json").write_text(json.dumps(catalog, indent=1) + "\n")

    all_kept = [vid for r in per_audition.values() for vid in r["kept"]]
    if args.worker_manifest:
        manifest = json.loads(Path(args.worker_manifest).read_text())
        by_voice = {r["voice"]: r for r in manifest["results"]}
        labels = {v["id"]: v["label"] for v in voices}
        missing = [vid for vid in all_kept if vid not in by_voice]
        if missing:
            fail("kept voices missing from the worker manifest: " + ", ".join(missing))
        for vid in all_kept:
            by_voice[vid]["label"] = labels[vid]
        (out / "worker-manifest.json").write_text(json.dumps(manifest, indent=1) + "\n")

    counts = Counter(v["label"].lower() for v in voices)
    summary = {"catalog_entries": len(voices), "built_in": len(builtin_ids), "extra": fixed - len(builtin_ids),
               "label_overrides_applied": applied,
               "label_overrides_unused": sorted(set(overrides) - {v["id"] for v in voices}),
               "duplicate_labels": sorted(k for k, n in counts.items() if n > 1),
               "auditions": per_audition}
    (out / "summary.json").write_text(json.dumps(summary, indent=1) + "\n")
    brief = {k: v for k, v in summary.items() if k != "auditions"}
    brief["auditions"] = {k: {x: (len(y) if isinstance(y, list) else y) for x, y in r.items() if x != "provenance"}
                          for k, r in per_audition.items()}
    print(json.dumps(brief))


if __name__ == "__main__":
    main()
