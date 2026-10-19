# ADR-0020: Pocket voice catalog and voice picker

- Status: Accepted
- Date: 2026-10-18

## Context

Listen offered 30 Pocket voices from a built-in list plus `POCKET_TTS_EXTRA_VOICES`, which holds at
most 32 entries and has no descriptive fields. Kyutai's CC0 voice donations add many more voices.
A flat drop-down with a hundred names gives no way to find a voice by gender or accent.

## Decision

- **Catalog file.** `POCKET_TTS_VOICE_CATALOG` names a JSON file
  `{"version":1,"voices":[{id,label,gainDb,gender,accent,style,source}]}` with at most 400 entries.
  An entry adds a Pocket voice (`pocket_<id>`) or replaces the label, gain and tags of a built-in or
  `POCKET_TTS_EXTRA_VOICES` voice with the same id. Only listed voices are accepted for synthesis.
- **Live reload, fail open.** Both runtimes re-read the file when its modification time or size
  changes, so adding auditioned voices needs no restart. A missing, oversized or invalid catalog is
  logged and ignored, and the built-in voices stay available.
- **Tags in the voice list.** `/:id/speech/voices` entries may carry
  `tags: {gender?, accent?, style?, source}`.
- **Picker.** With 12 or more voices, the Listen panel shows a search box, Female, Male and Saved
  filters, an accent menu and "Save this voice". Saved voices stay in the viewer's browser storage
  for every artifact. The current voice always stays in the list.
- **Worker.** The Pocket worker loads prepared voices from a manifest-driven directory
  (`ops/pocket-tts`). A voice can be loaded in the worker before the catalog exposes it, which is
  how auditions work.

## Consequences

- Adding or removing exposed voices is a file edit on the server host.
- Tags are produced by measurement (pitch for gender) and by the operator's audition choices, so
  they can be wrong. They help find a voice and do not restrict any.
