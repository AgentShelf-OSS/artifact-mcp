"""Reference implementation of the ADR-0023 stale-base guard (v1). Node and Rust must match it exactly.

Bodies are bytes. A single-file body is [(None, bytes)]; a bundle is [(path, bytes), ...].
"""
import hashlib
import json
import random

SPLIT = {0x3E, 0x3B, 0x7D, 0x0A}          # >  ;  }  \n   (the byte is kept at the end of its piece)
TRIM = {0x20, 0x09, 0x0A, 0x0D, 0x0C}     # space, tab, LF, CR, FF
MIN_LEN = 12


def _pieces(data: bytes):
    start = 0
    for i, b in enumerate(data):
        if b in SPLIT:
            yield data[start:i + 1]
            start = i + 1
    if start < len(data):
        yield data[start:]


def _trim(p: bytes) -> bytes:
    i, j = 0, len(p)
    while i < j and p[i] in TRIM:
        i += 1
    while j > i and p[j - 1] in TRIM:
        j -= 1
    return p[i:j]


def digests(files) -> set:
    out = set()
    for path, data in files:
        prefix = b"" if path is None else path.encode("utf-8") + b"\x00"
        for p in _pieces(data):
            t = _trim(p)
            if len(t) >= MIN_LEN:
                out.add(hashlib.sha256(prefix + t).digest()[:8])
    return out


def _range_text(base: int, current: int) -> str:
    return f"revision {current}" if base + 1 == current else f"revisions {base + 1}-{current}"


def message(base: int, current: int) -> str:
    return (f"Update is based on revision {base}, but the artifact is at revision {current}. "
            f"It would discard changes from {_range_text(base, current)}. "
            "Read the current revision and apply your change to it, use restore_artifact to roll back "
            "on purpose, or pass discard_newer: true.")


def evaluate(new_files, current_files, current_rev: int, history: dict, min_segments=20, min_share=0.10):
    """history: {rev: files} for retained revisions (rev < current_rev). Returns a result dict."""
    n = digests(new_files)
    c = digests(current_files)
    if not n or not c:
        return {"stale": False, "base": current_rev, "newer_segments": 0, "lost_segments": 0, "message": None}
    cands = {current_rev: c}
    for rev, files in history.items():
        cands[rev] = digests(files)
    best, best_i, best_u = None, 0, 1
    for rev in sorted(cands):                         # ascending, so a tie keeps the higher revision
        s = cands[rev]
        i, u = len(n & s), len(n | s)
        if best is None or i * best_u >= best_i * u:  # >= : later (higher) revision wins ties
            best, best_i, best_u = rev, i, u
    if best == current_rev:
        return {"stale": False, "base": best, "newer_segments": 0, "lost_segments": 0, "message": None}
    newer = c - cands[best]
    lost = newer - n
    stale = len(lost) >= min_segments and len(lost) >= min_share * len(newer)
    return {"stale": stale, "base": best, "newer_segments": len(newer), "lost_segments": len(lost),
            "message": message(best, current_rev) if stale else None}


# ---------------------------------------------------------------- synthetic vectors
def _doc(sections):
    lines = ["<!doctype html>", "<html lang=\"en\">", "<head><title>Synthetic report</title></head>", "<body>"]
    for name, count in sections:
        lines.append(f"<section id=\"{name}\">")
        rnd = random.Random(name)                      # fixed content per section
        for k in range(count):
            lines.append(f"  <p class=\"row\">{name} line {k} value {rnd.randint(1000, 9999)}</p>")
        lines.append("</section>")
    lines += ["<script>const total = 42; function show() { return total * 2; }</script>", "</body>", "</html>"]
    return ("\n".join(lines) + "\n").encode()


def single(b):
    return [(None, b)]


def build_vectors():
    r1 = _doc([("intro", 30)])
    r2 = _doc([("intro", 30), ("alpha", 25)])
    r3 = _doc([("intro", 30), ("alpha", 25), ("beta", 25)])
    r4 = _doc([("intro", 30), ("alpha", 25), ("beta", 25), ("gamma", 25)])
    r5 = _doc([("intro", 30), ("alpha", 25), ("beta", 25), ("gamma", 25), ("delta", 10)])
    edit = lambda b: b.replace(b"intro line 3 value", b"intro line 3 edited value")
    hist4 = {1: single(r1), 2: single(r2), 3: single(r3)}
    cases = [
        ("normal edit of current", single(edit(r4)), single(r4), 4, hist4),
        ("stale copy of r1 with an edit", single(edit(r1)), single(r4), 4, hist4),
        ("stale copy of r3, one newer revision", single(edit(r3)), single(r4), 4, hist4),
        ("intentional large deletion from current", single(_doc([("intro", 30), ("beta", 25), ("gamma", 25)])), single(r4), 4, hist4),
        ("older copy below the segment floor", single(r4), single(r5), 5, {**hist4, 4: single(r4)}),
        ("base older than retained history", single(edit(r1)), single(r4), 4, {3: single(r3)}),
        ("identical to current", single(r4), single(r4), 4, hist4),
        ("hand revert of the newest section (a revert, so flagged)", single(r3), single(r4), 4, hist4),
        ("tiny bodies are skipped", single(b"<p>hi</p>"), single(b"<p>there</p>"), 2, {1: single(b"<p>x</p>")}),
        ("bundle with a stale file",
         [("index.html", edit(r1)), ("app.js", b"const mode = 'old';\nfunction run() { return mode; }\n")],
         [("index.html", r4), ("app.js", b"const mode = 'new';\nfunction run() { return mode; }\n")], 4,
         {1: [("index.html", r1), ("app.js", b"const mode = 'old';\nfunction run() { return mode; }\n")],
          3: [("index.html", r3), ("app.js", b"const mode = 'new';\nfunction run() { return mode; }\n")]}),
    ]
    enc = lambda files: [{"path": p, "body": d.decode()} for p, d in files]
    out = []
    for name, new, cur, rev, hist in cases:
        res = evaluate(new, cur, rev, hist)
        out.append({"name": name, "current_revision": rev, "new": enc(new), "current": enc(cur),
                    "history": {str(k): enc(v) for k, v in hist.items()}, "expect": res})
    digest_examples = [
        {"path": None, "body": "<div class=\"a\">hello world</div>\n  short;\n<span>x</span>{a:b}\n",
         "digests": sorted(d.hex() for d in digests([(None, b"<div class=\"a\">hello world</div>\n  short;\n<span>x</span>{a:b}\n")]))},
        {"path": "pages/one.html", "body": "<main>first page body text</main>",
         "digests": sorted(d.hex() for d in digests([("pages/one.html", b"<main>first page body text</main>")]))},
    ]
    return {"version": 1, "min_segments": 20, "min_share": 0.10, "digest_examples": digest_examples, "cases": out}


if __name__ == "__main__":
    v = build_vectors()
    json.dump(v, open("stale-guard-vectors.json", "w"), indent=1)
    for c in v["cases"]:
        e = c["expect"]
        print(f"{c['name']:42} stale={e['stale']!s:5} base={e['base']} newer={e['newer_segments']} lost={e['lost_segments']}")
