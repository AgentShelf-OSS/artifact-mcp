# Voice-Zero narrator voices audition (2026-10-19)

Voices derived from Voice-Zero narrator clips (CC0) of volunteer LibriVox and
Archive.org readings. Each voice is named after its narrator.

On 2026-10-19 Neil approved Voice-Zero as a voice source and asked for an audition,
with more voices to follow after listening. The five prepared Voice-Zero voices
already live (Andy, Karen Savage, Kristin Hughes, Laura Caldwell, Simon Evers) came
from the earlier six-clip trial in `../voice-zero/`.

## Source and licence

- Repository: <https://github.com/OwenTyme/voice-zero>, pinned commit
  `490cfbee850a6d409076f477c766f567000a79b6`. `voices/` holds 154 FLAC clips
  (44.1 kHz, 5.7–15.0 s). `vz_fetch.py` read the git tree at that commit from the
  GitHub API and checked every downloaded file against its git blob SHA-1. The
  SHA-256 per clip is in `sources.json` and in each export row. The checksums of the
  six clips from the earlier trial match `../voice-zero/sources.tsv`.
- Licence: `LICENSE.md` puts every file under CC0 1.0 unless noted
  (<https://creativecommons.org/publicdomain/zero/1.0/>). The clips are cut from
  LibriVox readings (released into the public domain) and two Archive.org films.
- Credit: CC0 asks for none, but Neil asked to keep the narrator credit wherever
  these voices are listed. Each export row and worker manifest entry records
  `narrator`, `narrator_url` (LibriVox reader page), `reading`, `reading_url`,
  `source_file`, `source_commit`, `source_sha256`, `license: CC0-1.0`,
  `attribution`, and `"provenance": "Voice-Zero (CC0)"`. The audition card shows
  the narrator and the reading with links.
- Metadata: narrator, accent, and reading come from Voice-Zero's `voices/README.md`
  (SHA-256 in `sources.json`). Its author says the accent notes "are often based on
  guesswork". Four clips are not in that table under their exact file name
  (`alan_davis_drake2`, `emily_cripps2`, `jodi_krangle2`, `jodi_krangle-expressive`).
  They take the narrator row of the nearest file name; `readme_match` records this,
  and the card says the reading is the one listed for the other clip. The README
  misspells one narrator, so the label uses the LibriVox reader page spelling
  (Caden Vaughn Clegg). Labels must be printable ASCII: Délibáb is labelled Delibab,
  and the credit keeps the accents.

## Candidates

| Step | Count |
|---|---:|
| FLAC clips in `voices/` | 154 |
| Already exposed (Kyutai presets Peter Yearsley, Stuart Bell, Bill Boerst, Caro Davy; prepared Andy, Karen Savage 2, Kristin Hughes expressive, Laura Caldwell, Simon Evers) | 9 |
| Rejected by Neil earlier (Nicholas James Bridgewater) | 1 |
| Out of scope: `greasyplastic-whisper` (freesound.org whisper, not a LibriVox/Archive.org reading) and `rachel` (a child reader) | 2 |
| Candidates | 142 |

`voices-emotion/` (synthetic Chatterbox outputs) was never fetched.

## Pipeline (VM310, `/opt/docker/artifact-pocket-tts/trials/voice-zero-20261019`)

`run.sh <step>` holds the exact commands: default bridge only for `fetch`,
`--network none` for everything else, at most two 1-CPU containers at a time
(`analyze` is one container with two CPUs, as in the VCTK run).

1. `fetch`: `vz_fetch.py` on the host (standard library only).
2. `verify`: `../kyutai-donations-20261018/verify_export_procedure.py` reproduced the
   shipped `andrea.safetensors` byte for byte (`1191c25b…`) before the export.
3. `export`: `export_voicezero.py`, the donation export procedure unchanged: gain
   `min(-20 - RMS dBFS, 20*log10(0.9/peak))`, PCM16 at the source rate (44.1 kHz),
   gated `english_2026-04` weights (revision `19f95fe2…`), int8, `truncate=True`,
   one torch thread. The clips are used as published (no trimming or joining). The
   gated weights came offline from the donation trial's Hugging Face cache, so no
   token was used. 142 of 142 exported.
4. `jobs`, `synth`, `analyze`, `f0`: `synth.py`, `analyze.py`, and `f0.py` from the
   donation trial, unchanged: the two audition sentences, production (ungated)
   worker model, torch seed 20261018.
5. `select`: `select_voicezero.py`. The baseline for the 92 exposed voices (the live
   catalog, `catalog-live.json` on VM310) is the donation trial's and the VCTK
   trial's `embeddings.npz` (same model, seed, and sentences); every exposed voice
   has one. Writes `voice-metrics.json` (`"source": "voice-zero"`).
6. `../kyutai-donations-20261018/build_audition.py` with `audition.html` from this
   directory, then `publish_bundle.py` with the key `voices/pocket-voicezero-audition`.
7. `../kyutai-donations-20261018/stage_worker_voices.py` copied the 50 states to the
   worker voices directory and appended manifest entries (label = narrator name,
   `source: "voice-zero"`). Rollback copies: `voices/manifest.json.pre-voice-zero-20261019-112734`
   and `trials/voice-zero-20261019/rollback/manifest.json.pre-voicezero-20261019-112734`.
   Both workers were restarted one at a time. Each loads 215 voices (was 165) and
   returned audio for `pocket_vz_*` voices on :8790 and through :8794 (replica).

To roll back: copy the `pre-voice-zero` manifest over `voices/manifest.json`, delete
`voices/vz_*.safetensors`, and restart both workers one at a time.

## QC and selection

The thresholds are the donation audition's: mean WER ≤ 12% and any sentence ≤ 25%,
no clipping (≤ 0.1%), longest pause ≤ 2.0 s, edge silence ≤ 1.5 s, 120–250 wpm, and
speaker cosine < 0.975 to every exposed voice and to every better-scoring candidate.
The loudness target is the same −18.63 LUFS.

| Step | Count |
|---|---:|
| Candidates | 142 |
| Generation failed | 0 |
| Intelligibility (`mark_f_smith`, 65% WER on the neutral line) | 1 |
| Audio faults | 0 |
| Near-duplicate of an exposed voice | 20 |
| Near-duplicate of a better candidate | 19 |
| Passed all checks | 102 (29 female, 51 male, 22 not sure) |
| Not selected: American cap (20) | 44 |
| Not selected: 50-voice cap | 8 |
| Selected for audition | 50 |

All 50 selected voices have 0% WER on both sentences, 150–210 wpm, and gains of
+1 to +6 dB.

Most exposed-voice duplicates are other clips of narrators already live: all three
Jodi Krangle clips and Voicebynatalie sit near Kristin Hughes, Philippa near Karen
Savage, Alana Jordan near Laura Caldwell. The candidate pair cosine has a 99th
percentile of 0.970 and a maximum of 0.997 (two clips of one narrator).

Gender is the donation trial's estimate (Voice-Zero gives none): median F0 below
145 Hz is male, 175 Hz or above is female, otherwise not sure; it is also "not
sure" when the 7 nearest F0-confident voices (these candidates and the donation
and VCTK trial voices) disagree.
Balance: gender alternates, then within that gender the least-represented accent
comes first, then the best quality score. Voices with no confident estimate come
after both genders run out. American is capped at 40% (20 voices).

| Accent (Voice-Zero note → catalog) | Female | Male | Not sure | Total |
|---|---:|---:|---:|---:|
| American | 14 | 6 | 0 | 20 |
| English → British | 1 | 9 | 0 | 10 |
| Australian | 2 | 3 | 0 | 5 |
| Dutch | 1 | 1 | 0 | 2 |
| Austrian | 2 | 0 | 0 | 2 |
| Hungarian | 1 | 0 | 1 | 2 |
| Irish | 0 | 2 | 0 | 2 |
| Scottish | 0 | 1 | 1 | 2 |
| Irish, Northern → Northern Irish | 0 | 1 | 0 | 1 |
| Swedish | 0 | 1 | 0 | 1 |
| New Zealand | 1 | 0 | 0 | 1 |
| Chinese | 1 | 0 | 0 | 1 |
| German | 1 | 0 | 0 | 1 |
| **Total** | **24** | **24** | **2** | **50** |

Next batch: 52 passing voices were left out only by the caps: 44 American, and 8
with no confident gender estimate (Annise, Claudia Salto, Emily Cripps, Lucy
Burgoyne, Magdalena, Rosslyn Carlyle, Ruth Golding, Semantic Nuance). They are
already exported and analysed. Rerun `select` with a larger `--max-keep` or
`--accent-cap` to add them.

Names: the narrator's name, title-cased. When two clips of one narrator are both
selected, the variant clip carries its tag: Padraig O'hIceadha (Lyrical), Diana
Majlinger 2. Labels are unique against the live catalog, the VCTK audition, and
`../../catalog/label-overrides.json`. Voice ids are `vz_<clip file name>`.

## Audition

<https://artifact.neilblackman.dev/ub3etw1yqscp> (homelab, category Family, key
`voices/pocket-voicezero-audition`). It uses the same state format as the other
auditions: org state key `audition`, `{v:1,u,d:{<voice id>:{c,l,a,s,g,t}}}`. Accent
and gender are prefilled; the page stores `a`/`g` only when Neil changes them. Style
defaults to "audiobook". Each card credits the narrator and the reading.
`artifact-gates` passed 10 of 10 gates, and the state bridge harness saved a Keep,
a rename, and an accent change.

## Catalog

```sh
python3 ops/pocket-tts/voice_catalog.py \
  --audition ops/pocket-tts/trials/kyutai-donations-20261018/voice-metrics.json \
  --audition ops/pocket-tts/trials/vctk-20261019/voice-metrics.json \
  --audition ops/pocket-tts/trials/voice-zero-20261019/voice-metrics.json \
  --from-ct220 --worker-manifest ops/pocket-tts/voices/manifest.json --out /tmp/pocket-catalog
```

Kept voices get `source: "voice-zero"`, the narrator label unless Neil renamed the
voice, the prefilled gender and accent unless he changed them, and style
`audiobook` unless he set one. The five prepared Voice-Zero voices are built into
the generator (`PREPARED_VOICE_ZERO`), so they stay listed in every regeneration.
With no Voice-Zero choices, the output equals the live 92-voice catalog
(`/etc/artifact-mcp/pocket-voices.json` on CT220) apart from the trailing newline.

## Files

`sources.json` (all 154 clips: checksums, narrator, reading, and the excluded
clips with reasons), `export-manifest.json` (all 142 exports), and
`voice-metrics.json` (QC report and selection).

## Decision (2026-10-19)

Neil kept all 50 selected voices ("add all of them"). He had marked 8 in the
audition; `neil-choices-20261019.json` is the audition state with the other 42 set
to Keep, and it is the state used for the live catalog (142 voices). Regenerate with
`--state-file voices/pocket-voicezero-audition=trials/voice-zero-20261019/neil-choices-20261019.json`
(and state files for the other two auditions), not `--from-ct220`, unless the
audition page has been updated to match.
