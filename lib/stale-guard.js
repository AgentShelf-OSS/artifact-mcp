// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
// ADR-0023 stale-base guard: pure segment digests and base detection. Byte-for-byte parity with
// conformance/stale-guard-reference.py and the Rust runtime is required.
import { createHash } from "node:crypto";

const SPLIT = new Set([0x3e, 0x3b, 0x7d, 0x0a]); // >  ;  }  \n (kept at the end of the segment)
const TRIM = new Set([0x20, 0x09, 0x0a, 0x0d, 0x0c]); // space, tab, LF, CR, FF
const MIN_SEGMENT_BYTES = 12;
const NUL = Buffer.from([0]);

function addSegment(out, prefix, body, start, end) {
  while (start < end && TRIM.has(body[start])) start += 1;
  while (end > start && TRIM.has(body[end - 1])) end -= 1;
  if (end - start < MIN_SEGMENT_BYTES) return;
  const hash = createHash("sha256");
  if (prefix) hash.update(prefix);
  hash.update(body.subarray(start, end));
  out.add(hash.digest().subarray(0, 8).toString("hex"));
}

/**
 * Digest set of a body. `files` is an array of `{ path, body }`: `path` is null for a single-file
 * body and the stored relative path for a bundle file; `body` is a Buffer (a string is read as
 * UTF-8). Returns a Set of 16-character lowercase hex digests.
 */
export function segmentDigests(files) {
  const out = new Set();
  for (const file of files) {
    const body = Buffer.isBuffer(file.body) ? file.body : Buffer.from(String(file.body), "utf8");
    const prefix = file.path == null ? null : Buffer.concat([Buffer.from(String(file.path), "utf8"), NUL]);
    let start = 0;
    for (let i = 0; i < body.length; i += 1) {
      if (SPLIT.has(body[i])) {
        addSegment(out, prefix, body, start, i + 1);
        start = i + 1;
      }
    }
    if (start < body.length) addSegment(out, prefix, body, start, body.length);
  }
  return out;
}

export function staleMessage(base, current) {
  const range = base + 1 === current ? `revision ${current}` : `revisions ${base + 1}-${current}`;
  return `Update is based on revision ${base}, but the artifact is at revision ${current}. `
    + `It would discard changes from ${range}. `
    + "Read the current revision and apply your change to it, use restore_artifact to roll back "
    + "on purpose, or pass discard_newer: true.";
}

function intersectionSize(a, b) {
  const [small, large] = a.size <= b.size ? [a, b] : [b, a];
  let count = 0;
  for (const value of small) if (large.has(value)) count += 1;
  return count;
}

/**
 * Decide whether a full-content update is based on an older revision and would discard newer work.
 * `history` maps retained revision numbers (below `currentRevision`) to file arrays; revisions whose
 * bodies are unavailable must be left out by the caller.
 */
export function evaluateStaleBase({
  newFiles,
  currentFiles,
  currentRevision,
  history = new Map(),
  minSegments = 20,
  minShare = 0.1
}) {
  const notStale = (base) => ({ stale: false, base, newerSegments: 0, lostSegments: 0, message: null });
  const next = segmentDigests(newFiles);
  const current = segmentDigests(currentFiles);
  if (next.size === 0 || current.size === 0) return notStale(currentRevision);

  const candidates = new Map([[currentRevision, current]]);
  const entries = history instanceof Map ? history.entries() : Object.entries(history);
  for (const [revision, files] of entries) {
    const rev = Number(revision);
    if (rev === currentRevision) continue;
    candidates.set(rev, segmentDigests(files));
  }

  let best = null;
  let bestI = 0;
  let bestU = 1;
  // Ascending order with >= so a tie keeps the higher revision. Integer cross-multiplication
  // avoids floating-point ratios; set sizes are far below 2^26, so products stay exact.
  for (const revision of [...candidates.keys()].sort((a, b) => a - b)) {
    const set = candidates.get(revision);
    const i = intersectionSize(next, set);
    const u = next.size + set.size - i;
    if (best === null || i * bestU >= bestI * u) {
      best = revision;
      bestI = i;
      bestU = u;
    }
  }
  if (best === currentRevision) return notStale(best);

  const baseSet = candidates.get(best);
  let newer = 0;
  let lost = 0;
  for (const digest of current) {
    if (baseSet.has(digest)) continue;
    newer += 1;
    if (!next.has(digest)) lost += 1;
  }
  const stale = lost >= minSegments && lost >= minShare * newer;
  return {
    stale,
    base: best,
    newerSegments: newer,
    lostSegments: lost,
    message: stale ? staleMessage(best, currentRevision) : null
  };
}
