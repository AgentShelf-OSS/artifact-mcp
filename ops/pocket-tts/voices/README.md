# Voice-Zero presets

Five user-approved presets exported for English 2026-04 int8 with the timestamped
Pocket fork. Source recordings are listed as CC0 by [Voice-Zero](https://github.com/OwenTyme/voice-zero).
Sources, gain adjustments and SHA256 checksums are recorded in manifest.json and
../trials/voice-zero/sources.tsv. See ../trials/voice-zero-prepared/README.md for
the preparation and ungated-worker audition. Nicholas was excluded after listening.
These files load directly without gated weights or a Hugging Face token.

## Kyutai donated presets

Andrea, Darius, Hannah, and Bedfordshire were accepted by Neil in the first donated-voice audition. Their source clips are CC0; the Unmute contributor agreement explicitly covers voice transformation and commercial reuse subject to acceptable-use conditions. Exact source IDs, pinned revision, checksums, and consent URL are in manifest.json. `donated-reviews.json` separates measured technical evidence from Neil's listening acceptance; subjective ratings and use tags remain unset. All four run on the existing ungated worker model.

## VCTK candidates (not exposed)

Fifty `vctk_*` states were staged on 2026-10-19 for the VCTK audition. They are
loaded by the workers but hidden until Neil keeps them in the catalog. Voices
derived from the VCTK corpus, CSTR, University of Edinburgh, CC BY 4.0
(<https://doi.org/10.7488/ds/2645>). Each manifest entry records the speaker id,
the corpus members used, their checksums, speaker-info gender, accent, and region,
the licence, the attribution, and `"provenance": "VCTK (CC BY 4.0)"`. See
`../trials/vctk-20261019/README.md`.
