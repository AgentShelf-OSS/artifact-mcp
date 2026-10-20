"""Fetch the Voice-Zero narrator clips at a pinned commit and record their provenance.

Usage: vz_fetch.py <work-dir> [commit]
Standard library only (run with python3 -I on the host).

Reads the pinned git tree from the GitHub API, downloads voices/*.flac and
voices/README.md from raw.githubusercontent.com at that commit, and checks each
file against its git blob SHA-1 from the tree. Writes work-dir/src/<file> and
work-dir/sources.json with the SHA-256 per file and the narrator, accent, and
source reading parsed from voices/README.md.

The voices-emotion/ directory (synthetic Chatterbox outputs) is never fetched.
"""
import difflib, hashlib, json, re, sys, unicodedata, urllib.request
from pathlib import Path

REPO = "OwenTyme/voice-zero"
COMMIT = sys.argv[2] if len(sys.argv) > 2 else "490cfbee850a6d409076f477c766f567000a79b6"
W = Path(sys.argv[1]); SRC = W / "src"; SRC.mkdir(parents=True, exist_ok=True)
UA = {"user-agent": "pocket-voicezero-audition/1"}

# Already exposed (Kyutai presets or prepared Voice-Zero voices) or rejected by Neil.
EXPOSED = {"peter_yearsley": "exposed (Kyutai preset)", "stuart_bell": "exposed (Kyutai preset)",
           "bill_boerst": "exposed (Kyutai preset)", "caro_davy": "exposed (Kyutai preset)",
           "andy": "exposed (prepared Voice-Zero voice)", "karen_savage2": "exposed (prepared Voice-Zero voice)",
           "kristin_hughes-expressive": "exposed (prepared Voice-Zero voice kristin_hughes_expressive)",
           "laura_caldwell": "exposed (prepared Voice-Zero voice)", "simon_evers": "exposed (prepared Voice-Zero voice)",
           "nicholas_james_bridgewater": "rejected by Neil after listening"}
# Outside the approval (LibriVox/Archive.org readings) or not suitable for a narrator voice.
OUT_OF_SCOPE = {"greasyplastic-whisper": "freesound.org whisper clip, not a LibriVox/Archive.org reading",
                "rachel": "child reader (source accent note 'American, Child'); not cloned"}


def get(url):
    with urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=120) as r:
        return r.read()


def blob_sha1(data):
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


def ascii_fold(s):
    return unicodedata.normalize("NFKD", s).encode("ascii", "ignore").decode()


def title(name):
    name = re.sub(r"\.flac$", "", ascii_fold(name).strip())
    return " ".join(w[:1].upper() + w[1:] if w[:1].islower() else w for w in name.split())


def parse_readme(text):
    rows = []
    for line in text.splitlines():
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) < 4 or not cells[1] or cells[1].startswith(("File", "-")):
            continue
        m = re.match(r"\[(.+?)\]\((.*?)\)", cells[0])
        s = re.match(r"\[(.+?)\]\((.*?)\)", cells[3])
        rows.append({"narrator_raw": m.group(1) if m else cells[0], "reader_url": (m.group(2) or None) if m else None,
                     "files": [f.strip() for f in re.split(r"<br\s*/?>", cells[1]) if f.strip()],
                     "accent_raw": cells[2].strip(), "work": s.group(1) if s else cells[3],
                     "work_url": s.group(2) if s else None})
    return rows


def stem(f):
    return re.sub(r"(\.flac)+$", "", f)


def voice_id(st):
    v = re.sub(r"[^a-z0-9]+", "_", ascii_fold(st).lower()).strip("_")
    return ("vz_" + v)[:40].rstrip("_")


def variant(st, narrator, siblings):
    """Clip tag when a narrator has several clips: '-gravel' -> 'Gravel', 'x2' -> '2'.
    A hyphenated surname (coleman-hipkins) is not a tag."""
    m = re.search(r"-([a-z]+)$", st)
    if m and m.group(1) not in ascii_fold(narrator).lower():
        return m.group(1).title()
    m = re.search(r"[a-z](\d)$", st)
    return m.group(1) if m and siblings > 1 else None


tree = json.loads(get(f"https://api.github.com/repos/{REPO}/git/trees/{COMMIT}?recursive=1"))
if tree.get("truncated"):
    sys.exit("tree listing truncated")
blobs = {t["path"]: t["sha"] for t in tree["tree"] if t["type"] == "blob"}
flacs = sorted(p for p in blobs if re.fullmatch(r"voices/[^/]+\.flac", p))
readme = get(f"https://raw.githubusercontent.com/{REPO}/{COMMIT}/voices/README.md")
if blob_sha1(readme) != blobs["voices/README.md"]:
    sys.exit("README blob mismatch")
(W / "voices-README.md").write_bytes(readme)
lic = get(f"https://raw.githubusercontent.com/{REPO}/{COMMIT}/LICENSE.md")
if blob_sha1(lic) != blobs["LICENSE.md"]:
    sys.exit("LICENSE blob mismatch")
(W / "LICENSE.md").write_bytes(lic)
rows = parse_readme(readme.decode())
by_file = {stem(f): r for r in rows for f in r["files"]}

out, excluded = [], []
for path in flacs:
    fname = path.split("/", 1)[1]; st = stem(fname)
    data = get(f"https://raw.githubusercontent.com/{REPO}/{COMMIT}/{path}")
    if blob_sha1(data) != blobs[path]:
        sys.exit(f"blob mismatch for {path}")
    row, how = by_file.get(st), "exact"
    if row is None and re.sub(r"-[a-z]+$", "", st) in by_file:
        row, how = by_file[re.sub(r"-[a-z]+$", "", st)], "variant of " + re.sub(r"-[a-z]+$", "", st)
    if row is None:
        close = difflib.get_close_matches(st, list(by_file), n=1, cutoff=0.85)
        row, how = (by_file[close[0]], f"fuzzy ({close[0]})") if close else (None, "none")
    rec = {"voice": voice_id(st), "file": fname, "stem": st, "git_blob_sha1": blobs[path],
           "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data),
           "source_url": f"https://raw.githubusercontent.com/{REPO}/{COMMIT}/{path}",
           "readme_match": how}
    if row:
        rec.update({"narrator": title(row["narrator_raw"]), "narrator_raw": row["narrator_raw"],
                    "reader_url": row["reader_url"], "accent_raw": row["accent_raw"],
                    "work": row["work"], "work_url": row["work_url"]})
    reason = EXPOSED.get(st) or OUT_OF_SCOPE.get(st)
    if reason:
        excluded.append({**rec, "reason": reason})
        continue
    if not row:
        sys.exit(f"no README row for {fname}")
    (SRC / fname).write_bytes(data)
    out.append(rec)
    print(json.dumps({"voice": rec["voice"], "narrator": rec.get("narrator"), "match": how}), flush=True)

# Variant tags need every clip of the narrator, exposed ones included.
per_narrator = {}
for r in out + excluded:
    per_narrator[r.get("narrator_raw")] = per_narrator.get(r.get("narrator_raw"), 0) + 1
for r in out + excluded:
    r["variant"] = variant(r["stem"], r.get("narrator_raw") or "", per_narrator[r.get("narrator_raw")]) if r.get("narrator_raw") else None
ids = [r["voice"] for r in out]
if len(set(ids)) != len(ids):
    sys.exit("duplicate voice ids")
(W / "sources.json").write_text(json.dumps({
    "repo": f"https://github.com/{REPO}", "commit": COMMIT, "license": "CC0-1.0",
    "license_url": "https://creativecommons.org/publicdomain/zero/1.0/",
    "license_text_sha256": hashlib.sha256(lic).hexdigest(),
    "readme_sha256": hashlib.sha256(readme).hexdigest(),
    "flac_in_voices": len(flacs), "candidates": out, "excluded": excluded}, indent=1))
print("candidates", len(out), "excluded", len(excluded), "flac", len(flacs))
