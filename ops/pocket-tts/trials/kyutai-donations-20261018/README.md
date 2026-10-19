# Kyutai voice donations audition (2026-10-18)

Source: Hugging Face `kyutai/tts-voices`, folder `voice-donations`, pinned revision
`323332d33f997de8394f24a193e1a76df720e01a` (the same revision as the first donated
batch). License CC0-1.0. Consent: the Unmute contributor agreement,
<https://kyutai.org/legal/Terms%20of%20Use%20-%20Unmute%20Voice%20Donation%20Project%20v1.pdf>.
Only the `*_enhanced.wav` clips are used (228 donors). Andrea, Darius, Hannah, and
Midlands_Bedfordshire_Dialect were accepted earlier and are skipped: 224 candidates.
All 228 downloads matched their LFS SHA-256.

## Pipeline (VM310, `/opt/docker/artifact-pocket-tts/trials/donations-20261018`)

1. `export_all.py` – voice states with the gated `kyutai/pocket-tts` English 2026-04
   weights (revision `19f95fe2…`), int8, one torch thread. Gain before encoding:
   `min(-20 - RMS dBFS, 20*log10(0.9/peak))`, written as PCM16 at the source rate,
   `truncate=True`. `verify_export_procedure.py` reproduces the shipped
   `andrea.safetensors` byte for byte (`1191c25b…`). The HF token came from the
   homelab secrets manager into a RAM file; it is not stored in the trial dir.
2. `synth.py` – both sentences per voice with the production (ungated, int8) model,
   torch seed 20261018. Also run for the 35 existing voices as a baseline.
3. `analyze.py` – faster-whisper `small.en` WER, LUFS, peak, clipping, edge
   silences, longest pause, words per minute, and a WavLM speaker embedding.
   `f0.py` – median F0 with Praat (75–500 Hz).
4. `select_candidates.py` – filters and ranks; writes `voice-metrics.json`.
5. `build_audition.py` + `audition.html` – bundle with 32 kbps MP3 data URIs in
   `audio-NN.js` chunks (8 MiB bundle limit); `publish_bundle.py` publishes it.
6. `stage_worker_voices.py` – copies the 80 selected states into the worker voice
   directory and appends manifest entries. The worker loads them; artifact-mcp
   exposes none until the catalog lists them.

## Result

| Step | Count |
|---|---:|
| Candidates | 224 |
| Generation failed (no end of speech) | 2 (`donor_92f0`, `latin_accent`) |
| Intelligibility (WER) | 1 (`robert`, 65% on the neutral line) |
| Audio faults (pause > 2 s or edge silence > 1.5 s) | 5 |
| Near-duplicate of an exposed voice (cosine ≥ 0.975) | 6 |
| Near-duplicate of a better candidate | 23 |
| Passed all checks | 187 |
| Selected for audition | 80 |

Limits sit just outside the range of the 35 accepted voices. Those voices have a
maximum pause of 1.9 s, edge silence up to 1.24 s, a rate of 171–245 wpm, and a
maximum distinct-speaker cosine of 0.97. Known same-donor pairs score 0.975–0.986.
All 80 selected voices have a mean WER of 2% or less and no clipping.

The pool is mostly male (155 male, 10 female, 22 unsure among passing voices).
Selection keeps every passing female or unsure voice, then the best-scoring male
voices: 10 female, 48 male, and 22 unsure estimates.

Gender: median F0 < 145 Hz gives male and ≥ 175 Hz gives female, otherwise null.
The estimate is also null when the voice's 7 nearest speaker-embedding neighbours
(F0-confident voices) disagree. These are estimates; the audition lets Neil override.

Gain: the loudness target is the median of `LUFS + current gain` over the 30
exposed voices, −18.63 LUFS. The method reproduces their hand-set gains with a
0.64 dB median residual. New gains are `target − LUFS`, rounded to 0.5 dB and
clamped to ±12 dB. The Listen player's −3 dB limiter handles peaks.

Audition: <https://artifact.neilblackman.dev/h27m3ptw1wr0> (homelab, category
Family, key `voices/pocket-donated-audition`). Choices persist in org viewer
state key `audition` as `{v:1,u,d:{<voice id>:{c:keep|maybe|skip,l,a,s,g,t}}}`.

## Catalog

`../../voice_catalog.py` reads the choices from CT220 (read-only) and writes
`pocket-voices.json`:

```sh
python3 ops/pocket-tts/voice_catalog.py \
  --metrics ops/pocket-tts/trials/kyutai-donations-20261018/voice-metrics.json \
  --from-ct220 --worker-manifest ops/pocket-tts/voices/manifest.json --out /tmp/pocket-catalog
```

`../../catalog/pocket-voices.json` is the baseline output with no choices yet: the
21 built-in presets and the 9 `POCKET_TTS_EXTRA_VOICES`.

Since 2026-10-19 the generator merges several auditions (repeat `--audition`; the
VCTK batch is in `../vctk-20261019`). It also applies `../../catalog/label-overrides.json`
last, which keeps Neil's renames (`donor_30c5` Mason, `donor_3973` Clara,
`donor_8935` Owen). The command above still works.
