# 16. Stale-base guard for full-content updates

Date: 2026-10-16

## Status

Accepted

## Context

A full-content update replaces the whole body of an artifact. Three paths do this:
`update_artifact` with `html` or `files`, and `publish_artifact` or `publish_bundle` with an
`artifact_key` that already names an artifact (ADR-0015).

The server checks the revision only when the caller sends `expected_revision`. That check proves
that the caller knew the current revision number. It does not prove that the new body was made from
the current body. An agent can read the revision number, then upload a body that it built from an
old local copy. A keyed publish never sends a revision: the server uses the revision that its lookup
read.

This failure happened in production on 2026-10-16. Several sessions changed one artifact directly on
the server over nine revisions, but its local source was not updated. A later session built from that
stale local source and published. The update passed every check and removed nine revisions of work
from the live artifact. The operator saw the loss and used `restore_artifact`.

The risk increases as more publishers use artifact keys, because keyed publication is designed for
agents that come back in a later session without the id.

`patch_artifact` does not have this risk. Its edits apply to the current body, and
`expected_revision` is required.

## Decision

Before a full-content update, the server finds the revision that the new body is based on. If that
revision is not the current revision, and the update removes a meaningful part of the work that was
added after it, the update is **stale**. This release warns about stale updates. A later release can
refuse them by changing the default mode.

### Scope

- The guard applies to `update_artifact` with `html` or `files`, and to keyed `publish_artifact`
  and `publish_bundle` calls that update an existing artifact.
- It does not apply to metadata-only or entry-only updates, `patch_artifact`, `restore_artifact`,
  or a publication that creates an artifact.
- It runs after authentication, the role check, and the ownership policy, so its result cannot
  reveal anything about an artifact that the caller cannot update (invariant 3).
- If the caller sends an `expected_revision` that is not the current revision, the guard does not
  run. The existing revision-conflict error is returned.

### Segments

The guard compares sets of segment digests. Both runtimes compute them on bytes, as follows:

1. Split the body after each `>` (0x3E), `;` (0x3B), `}` (0x7D), and line feed (0x0A). The split
   byte stays at the end of its segment. The remaining bytes after the last split byte are one more
   segment.
2. Trim leading and trailing space (0x20), tab (0x09), line feed (0x0A), carriage return (0x0D), and
   form feed (0x0C).
3. Keep a trimmed segment only if it has 12 or more bytes.
4. The digest is the first 8 bytes of SHA-256 of the trimmed segment. For a bundle file, the digest
   input is the UTF-8 file path, one 0x00 byte, then the trimmed segment.
5. A body's digest set is the union over its files.

### Base and stale

- If the new set or the current set is empty, the update is not stale.
- The candidates are the current revision and each retained revision whose body is available.
- For each candidate, `i` = size of the intersection with the new set and `u` = size of the union.
  The base is the candidate with the highest `i / u`. Compare `i1 * u2` with `i2 * u1` as integers.
  A tie goes to the higher revision.
- If the base is the current revision, the update is not stale.
- Otherwise `newer` = current set minus base set, and `lost` = `newer` minus the new set.
- The update is stale when `|lost| >= STALE_GUARD_MIN_SEGMENTS` (default 20) **and**
  `|lost| >= STALE_GUARD_MIN_SHARE * |newer|` (default 0.10, compared as a 64-bit float).

### Result

The text for a stale update is fixed:

> Update is based on revision {base}, but the artifact is at revision {current}. It would discard
> changes from {range}. Read the current revision and apply your change to it, use restore_artifact
> to roll back on purpose, or pass discard_newer: true.

`{range}` is `revision {current}` when `base + 1 = current`, otherwise
`revisions {base+1}-{current}`.

- **Mode.** `STALE_GUARD=warn|enforce|off`. The default is `warn`.
  - `warn`: the update continues. Its result adds `warnings`, an array with the text above. The
    result has no `warnings` field when the update is not stale, so other results are unchanged.
  - `enforce`: the update is refused with a tool error that has the text above. Nothing changes.
  - `off`: the guard does not run.
- **Override.** `update_artifact`, `publish_artifact`, and `publish_bundle` accept an optional
  boolean `discard_newer`. When it is `true`, a stale update continues with no warning. Any other
  value than a boolean is an invalid-params error: `discard_newer must be a boolean`.
- **Log.** Each stale detection writes one log line with the artifact id, the base and current
  revisions, `|newer|`, `|lost|`, the mode, and whether `discard_newer` was set. It does not log
  segment content. These lines measure the false-positive rate before `enforce` becomes the default.

### Parity

Node and Rust implement the same segmentation, digests, comparison, thresholds, text, and modes.
`conformance/stale-guard-vectors.json` holds synthetic test vectors made by
`conformance/stale-guard-reference.py`. Both test suites must reproduce every vector exactly. Real
artifact bodies are not committed.

## Evidence

A prototype ran against two real artifacts with their full retained history (20 and 10 revisions).

| Case | Result |
| --- | --- |
| The 2026-10-16 incident (stale body over revision 22) | Stale: base 13, would discard 14-22 |
| A stale local copy of a second artifact over its current revision | Stale: base 9, would discard 10-11 |
| 19 normal sequential edits across both artifacts | Not stale |
| A restore made with `update_artifact` that dropped 17 segments | Not stale (below the floor of 20) |

The Python prototype took about 180 ms for a 350 KB body and 20 revisions.

## Consequences

- A publisher that builds from a stale copy learns about it at once, and in `enforce` mode cannot
  silently remove other people's work. No client change is necessary.
- A rollback done by hand, for example removing exactly the section that the last revision added,
  produces a copy of an older revision and is reported as stale. The text names `restore_artifact`
  and `discard_newer`.
- If the true base is older than the retained history, the most similar retained revision becomes
  the base. A very old copy is still usually reported.
- The input schemas of `update_artifact`, `publish_artifact`, and `publish_bundle` gain
  `discard_newer`, and their output schemas gain the optional `warnings` array. The frozen
  tool-definition golden is recorded again from Node.
- Each full-content update reads up to `MAX_HISTORY` retained bodies. A runtime may cache digest
  sets by body hash.
- The thresholds are heuristics. Operators can change them by environment variable.
- A follow-up change makes `enforce` the default after the log lines show a low false-positive rate.

## Alternatives considered

- **Require `expected_revision` on every update.** This breaks existing clients and does not stop
  the incident: the stale publisher read the current revision number just before it wrote.
- **Refuse large shrinks only.** A stale body is not always smaller, and a normal large deletion
  would be refused. It also cannot tell the publisher which work it would remove.
- **Track the revision that each session last read.** Scripted publishers read the current revision
  immediately before they write, so the tracked revision is always current.
- **Record audit events.** Only the Rust runtime has an audit log. A log line gives the same
  measurement in both runtimes without changing the audit schema.
