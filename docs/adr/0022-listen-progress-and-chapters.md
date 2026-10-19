# ADR-0022: Listen progress, chapters and saved place across devices

- Status: Accepted
- Date: 2026-10-19
- Builds on: ADR-0007 and ADR-0008 (viewer state), ADR-0019 (Listen on the native host),
  ADR-0021 (car media controls)

## Context

Inside the Android app, Listen mirrors only a state, a title and a few buttons (ADR-0019,
ADR-0021). The car screen and the media notification cannot show how far into a chapter the
listener is, cannot list chapters, and show "paused" with no reason when speech fails or the
connection drops. The saved place lives only in one browser's `localStorage`, so a book started
in the car does not continue on the phone or the desktop.

Apps released before this change reject unknown message fields and unknown enum values, so every
addition must be gated by a capability the app advertises.

## Decision

### The `media.progress` contract (bridge protocol v1)

This text is binding for the shell and the Android app. The app is changed against exactly this
text.

New capability string: `"media.progress"`. The app advertises it in its capabilities list. The
shell sends any field below ONLY when the host advertises `"media.progress"` (older apps reject
unknown fields and unknown enum values). Apps without it see today's messages unchanged.

**shell → app: `media.session` (existing message, extended)**

Existing: `{v,type:"media.session",artifactId,state,title,artist,album,actions[],rate?}`

Additions when the host has `media.progress`:

- `state`: may also be `"buffering"` (the shell wants to play but is loading, reconnecting or
  restoring).
- `position`: number, seconds, 0..86400, at most 1 decimal. Estimated position inside the current
  chapter (or the whole reading scope when there are no chapters). Estimated from synthesized
  durations where known, else from text length at the current rate.
- `duration`: number, seconds, 0..86400, at most 1 decimal. Estimated length of the same span.
  Omit if unknown.
- `chapterIndex`: integer ≥ 0, index into the last `media.chapters` list. Omit when there are no
  chapters.
- `error`: string, 1..120 characters, a short human reason when Listen stopped on a failure (for
  example `"Speech is unavailable"`, `"No connection"`). Only with state `"paused"` or `"none"`.
- `actions`: may also contain `"forward"` (skip ahead 30 s) and `"chapter"` (jump to a chapter by
  index). The maximum number of actions becomes 12 for apps with `media.progress`.

The shell throttles position updates: it sends `media.session` on a state, chapter or title change
and at most every 5 s otherwise. The app interpolates the position while playing
(`position + elapsed * rate`).

**shell → app: `media.chapters` (new message, only with `media.progress`)**

`{v,type:"media.chapters",artifactId,chapters:[string,...]}`

- `chapters`: 0..500 titles, each 1..200 characters. Sent when the chapter list changes (and after
  hello/ready). An empty list means no chapters.

**app → shell: `media.command` (existing message, extended)**

Existing: `{v,type:"media.command",artifactId,action}`

New actions (sent only by apps with `media.progress`):

- `"forward"`: skip ahead 30 s (within the current block where possible, else the next block).
- `"chapter"`: requires an extra integer field `index` (0..499): start reading that chapter.

### How the shell fills the contract

- `position` and `duration` are media seconds at 1×. Played blocks use their synthesized length.
  Other blocks use their text length at a rate calibrated on the blocks already heard (15
  characters a second until at least 5 s are known). With chapters, the reading queue is the
  current chapter, so both values are chapter-relative.
- The chapter list is the reader outline's chapter list. When the host has `media.progress`, the
  shell requests the outline even while the player is closed, so a car-started narration has
  chapters. `chapterIndex` is the current chapter's position in that list.
- `"buffering"` is sent while Listen wants to play and is extracting text, waiting for audio,
  restoring a saved place or retrying a dropped speech stream. `navigator.mediaSession` has no
  buffering state, so the browser keeps `"playing"` there; the player's status line says
  "Reconnecting".
- `error` is set when Listen stops on a failure: speech unavailable or busy, no connection (after
  the retries give up), audio playback failure, unreadable or too-long pages, and a saved place
  that no longer matches. Starting again or pressing Stop clears it.
- `forward` mirrors the 15-second rewind. It seeks within the current block's received audio when
  it can. Otherwise it carries the remainder into the following blocks, using known durations and
  then the estimate above. The Media Session `seekforward` action does the same; it used to skip a
  whole block.
- An app without `media.progress` keeps receiving byte-identical messages: `"buffering"` is sent
  to it as `"playing"`, and none of the new fields, actions or messages reach it.
  `test/shell-native.test.js` mirrors the app's field allowlists for both kinds of app.

### Saved place across devices

The Listen checkpoint (the `localStorage` entry `artifact-reader-place:[artifact, viewer, page]`)
gains a `savedAt` timestamp, which changes only when the checkpoint changes. For signed-in viewers
of the current revision, the shell also stores it in viewer-scope state (ADR-0008) under the key
`listen-place` of the artifact:

```json
{"version":1,"pages":{"<bundle page, or empty for a single page>":{"savedAt":1760900000000,"place":{"version":2,"current":{},"scopes":{}}}}}
```

- **Load.** When Listen's voices load, the shell reads the key once. For each page, the newer copy
  by `savedAt` wins: a newer server copy replaces the local cache, and a newer local copy (for
  example saved offline) is uploaded. Resume, Play with nothing loaded (page and section modes,
  v1.24.0) and car autoplay wait up to 2.5 s for this read.
- **Write.** Writes are unconditional `PUT`s of the whole value. While playing, at most one write
  every 30 s. Pause, Stop, hiding the page and leaving it write at once (with `keepalive`), but
  never more than one write every 2 s. A `429` pauses writes for 60 s.
- **Bounds.** At most 16 pages; the value stays under 48 KB (the oldest pages go first, then the
  per-scope history), well inside the 256 KB state value limit and the `keepalive` body limit. One
  key per artifact, out of the 64-key viewer-scope budget.
- **Finished.** Finishing a reading removes the page from both copies (the key is deleted when no
  page is left).
- **Fail open.** Historical views, signed-out viewers, a disabled state store, `4xx` answers and
  network failures leave Listen on its local copy, as before. Bundles keep their per-page
  semantics: each page has its own entry.

### Read-ahead and speeds

- Listen reads ahead at least 3 blocks or about 60 s of listening at the current speed, whichever
  is more, up to 8 blocks and 24 MB of audio. One viewer runs at most 2 speech requests at once:
  the block about to play takes a slot from the farthest read-ahead. A read-ahead stream is read to
  the end so that its worker slot (`TTS_WORKER_CONCURRENCY`) is released; it then plays from
  memory. A `429` on read-ahead is not retried; read-ahead pauses with exponential backoff (2 s to
  30 s), and the block is requested again, with the normal retries, when it plays.
- The speed list is 0.75×, 0.8×, 0.9×, 1×, 1.1×, 1.25×, 1.5×, 1.75×, 2×, 2.5× and 3×. Saved places
  accept every listed speed. The car speed button (ADR-0021) cycles 1×, 1.25×, 1.5×, 1.75× and 2×.

## Consequences

- Android Auto and the media notification can show a progress bar, a chapter list and a failure
  reason, and offer "forward 30 s", without changing older apps.
- Position is an estimate until blocks are synthesized; a progress bar can move unevenly at chapter
  start. The app interpolates between updates at most 5 s apart.
- A saved place follows the viewer across browsers and devices. Two devices playing the same
  artifact at once overwrite each other; the last write wins.
- Each viewer page load with Listen makes one extra viewer-state read; listening adds about two
  writes a minute. Both count towards `INGRESS_STATE_PER_WINDOW` for that source.
- Read-ahead uses more worker time up front and up to 24 MB of memory per player, in exchange for
  fewer gaps on slow or unstable connections.
