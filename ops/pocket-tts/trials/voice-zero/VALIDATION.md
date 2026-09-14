# Trial status

The six Voice-Zero references were downloaded from the pinned URLs in
`sources.tsv` on 2026-09-14. SHA-256 values are recorded there.

All files were verified as finite mono FLAC audio at 44.1 kHz:

| ID | Duration |
| --- | ---: |
| `andy` | 8.998 s |
| `karen_savage2` | 9.040 s |
| `kristin_hughes_expressive` | 8.684 s |
| `laura_caldwell` | 8.640 s |
| `nicholas_james_bridgewater` | 16.466 s |
| `simon_evers` | 10.534 s |

Generation was blocked before synthesis. The verified worker image contains
the ungated `kyutai/pocket-tts-without-voice-cloning` weights. Pocket TTS
rejects arbitrary reference audio with those weights and requires access to
the gated `kyutai/pocket-tts` model. No production container or configuration
was changed. The isolated container and VM310 staging directory were removed
after the failed check.

For a control sample, the same isolated job successfully generated
`audio/alba_baseline.wav` with `pocket-tts-timestamped-65037e84-english_2026-04-int8`.
It is 24 kHz, 4.080 seconds, 195,884 bytes, SHA-256
`6f038ea2498d14e0490182bee5bc583cf3779fd8b5fdd94a79a00f23d3920d4b`.
