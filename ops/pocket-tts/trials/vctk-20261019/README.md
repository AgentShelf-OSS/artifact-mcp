# VCTK accented voices audition (2026-10-19)

Voices derived from the VCTK corpus, CSTR, University of Edinburgh, CC BY 4.0.

The Kyutai donation pool was mostly male and mostly Indian-English. This batch adds
accent and gender variety from the CSTR VCTK Corpus.

## Source and licence

- Corpus: Junichi Yamagishi, Christophe Veaux, Kirsten MacDonald, *CSTR VCTK Corpus:
  English Multi-speaker Corpus for CSTR Voice Cloning Toolkit (version 0.92)*,
  University of Edinburgh, Centre for Speech Technology Research (CSTR), 2019.
  DOI <https://doi.org/10.7488/ds/2645>, Edinburgh DataShare handle 10283/3443.
- Licence: Creative Commons Attribution 4.0 International (CC BY 4.0),
  <https://creativecommons.org/licenses/by/4.0/>. Attribution is required. Each
  worker manifest entry carries `license`, `attribution`, `source_doi`, and
  `"provenance": "VCTK (CC BY 4.0)"`, and the audition page shows the credit.
- Changes made (CC BY asks for them to be indicated): 3–5 recordings per speaker
  are joined into one 10–15 s reference clip and level-normalised. Pocket TTS
  conditions on that clip and speaks new text in a similar voice. No recording is
  redistributed, except `speaker-info.txt` (copied here unchanged).
- Archive: `VCTK-Corpus-0.92.zip`, 11,747,302,977 bytes, MD5
  `8a6ba2946b36fcbef0212cad601f4bfa` (equal to the DataShare ETag), SHA-256
  `f96258be9fdc2cbff6559541aae7ea4f59df3fcaf5cf963aae5ca647357e359c`.
  `vctk_fetch.py` read only the needed members with HTTP range requests, each one
  checked against its CRC-32 in the zip central directory. The full archive was
  then downloaded once to confirm the checksums. All 359 members used in the
  references matched it (SHA-256 per member in `refs.json`). The archive was
  deleted after the check.
- Consent: the corpus record says it "was recorded for the purpose of building
  HMM-based text-to-speech synthesis systems, especially for speaker-adaptive
  HMM-based speech synthesis". The release is CC BY 4.0. We hold no per-speaker
  written consent for voice cloning. `../../README.md` asks for such consent for
  cloned voices. These voices were prepared at Neil's request; keeping any of them
  is his decision.

## Pipeline (VM310, `/opt/docker/artifact-pocket-tts/trials/vctk-20261019`)

`run.sh <step>` holds the exact container commands: default bridge or
`--network none`, at most two 1-CPU containers at a time.

1. `mklist.py` + `vctk_fetch.py`: 12 utterances per speaker (60th–95th size
   percentile, evenly spaced), both microphones, and transcripts.
2. `build_refs.py`: per utterance peak, clipped samples, noise floor, and SNR.
   mic2 (MKH 800) unless mic1 is better by more than 3 dB median SNR (only p227).
   p280 and p315 have no mic2 and use mic1 with a 60 Hz high-pass (the corpus
   notes low-frequency noise on mic1). The highest-SNR unclipped utterances are
   added until the clip reaches 10 s (3–5 utterances, at most 15 s; two when they
   already reach 10 s, only p306). Edges are cut to 0.2 s of room tone, with 10 ms
   fades. 110 of 110 speakers produced a clip (10.1–15.0 s).
3. `export_vctk.py`: the donation export procedure unchanged: gain
   `min(-20 - RMS dBFS, 20*log10(0.9/peak))`, PCM16 at 48 kHz, gated
   `english_2026-04` weights (revision `19f95fe2…`), int8, `truncate=True`, one
   torch thread. Before the export, `../kyutai-donations-20261018/verify_export_procedure.py`
   reproduced the shipped `andrea.safetensors` byte for byte (`1191c25b…`). The
   gated weights came offline from the donation trial's Hugging Face cache, so no
   token was used. Exports are deterministic (p225 hashed the same twice).
4. `synth.py`, `analyze.py`, and `f0.py` from the donation trial, unchanged: the two
   audition sentences, production (ungated) worker model, torch seed 20261018.
   The baseline for the 37 exposed voices is the donation trial's
   `embeddings.npz` (same model, seed, and sentences).
5. `select_vctk.py`: filters, de-duplicates, balances, and names the voices; writes
   `voice-metrics.json` (`"source": "vctk"`).
6. `../kyutai-donations-20261018/build_audition.py` with `audition.html` from this
   directory, then `publish_bundle.py` with the VCTK key.
7. `../kyutai-donations-20261018/stage_worker_voices.py` copied the 50 states to
   the worker voices directory and appended manifest entries (label = suggested
   name, `source: "vctk"`). Rollback copies: `voices/manifest.json.pre-vctk-20261019-023611`
   and `trials/vctk-20261019/rollback/`. Both workers were restarted one at a time.
   Each loads 165 voices. Synthesis returned audio on :8790 and :8794.

## QC and selection

The thresholds are the donation audition's: mean WER ≤ 12% and any sentence ≤ 25%,
no clipping (≤ 0.1%), longest pause ≤ 2.0 s, edge silence ≤ 1.5 s,
120–250 wpm, and speaker cosine < 0.975 to every exposed voice and to every
better-scoring candidate. The loudness target is the same −18.63 LUFS.

| Step | Count |
|---|---:|
| Speakers (VCTK 0.92) | 110 |
| Exported and synthesized | 110 |
| Generation failed / intelligibility | 0 / 0 |
| Audio faults (pause 2.04–2.16 s) | 6 (p293, p299, p301, p305, p308, p362) |
| Near-duplicate of an exposed voice | 14 |
| Near-duplicate of a better candidate | 9 |
| Passed all checks | 81 (41 female, 40 male) |
| Selected for audition | 50 |

All 50 selected voices have 0% WER on both sentences.

Most duplicates were Southern-English women close to Anna, Vera, or Fantine, and
Southern-English men close to Charles or Paul. The candidate pair cosine has a 99th
percentile of 0.967 and a maximum of 0.986.

Balance: gender alternates, then within that gender the least-represented accent
comes first, then the best quality score. The accent cap is 25% (12 voices); the
spread rule kept every accent well below it.

| Accent (speaker-info → catalog) | Female | Male | Total |
|---|---:|---:|---:|
| Scottish | 2 | 6 | 8 |
| English → British | 5 | 2 | 7 |
| American | 3 | 4 | 7 |
| Irish | 3 | 3 | 6 |
| Canadian | 3 | 3 | 6 |
| NorthernIrish → Northern Irish | 3 | 2 | 5 |
| Indian | 1 | 2 | 3 |
| SouthAfrican → South African | 2 | 1 | 3 |
| Australian | 0 | 2 | 2 |
| Welsh | 1 | 0 | 1 |
| NewZealand → New Zealand | 1 | 0 | 1 |
| Unknown (p280, France) → none | 1 | 0 | 1 |
| **Total** | **25** | **25** | **50** |

Gender and accent come from `speaker-info.txt`. F0 is a sanity check only. It
agrees for 45 voices and is borderline (145–175 Hz) for 5 women. It contradicts
speaker-info for none.

Names: friendly first names that fit the accent and gender (for example Fiona,
Callum, Siobhan, Rhian), unique against every label in the installed catalog,
every Kyutai audition candidate, and `../../catalog/label-overrides.json`. The
speaker id stays in the worker manifest (`source_id`) and on the audition card.

## Audition

<https://artifact.neilblackman.dev/tjrfxp6iyhmx> (homelab, category Family, key
`voices/pocket-vctk-audition`). It uses the same state format as the Kyutai
audition: org state key `audition`, `{v:1,u,d:{<voice id>:{c,l,a,s,g,t}}}`.
Accent and gender are prefilled from speaker-info. The page stores `a`/`g` only
when Neil changes them. It also has an accent filter and an accent sort.
`artifact-gates` passed 10 of 10 gates.

## Catalog

```sh
python3 ops/pocket-tts/voice_catalog.py \
  --audition ops/pocket-tts/trials/kyutai-donations-20261018/voice-metrics.json \
  --audition ops/pocket-tts/trials/vctk-20261019/voice-metrics.json \
  --from-ct220 --worker-manifest ops/pocket-tts/voices/manifest.json --out /tmp/pocket-catalog
```

Kept VCTK voices get `source: "custom"` (artifact-mcp accepts only
`pocket-preset|kyutai-donation|voice-zero|custom`). They also get the speaker-info
gender and accent unless Neil changed them, and the suggested name unless he
renamed the voice. With no VCTK choices, the output is the installed 37-voice
catalog byte for byte (`pocket-voices.audition-1b.json`). The only difference is
the trailing newline.

## Files

`export-manifest.json` (all 110 exports), `refs.json` (chosen utterances, texts,
member checksums, and microphone choice per speaker), `voice-metrics.json` (QC report and
selection), and `speaker-info.txt` (corpus copy, CC BY 4.0).
