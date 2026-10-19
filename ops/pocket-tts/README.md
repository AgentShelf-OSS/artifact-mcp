# Pocket TTS comparison worker

The pinned `pocket-tts-timestamped` fork of Pocket TTS 3.1.0 runs the
`english_2026-04` model on CPU. It preserves the ordinary Pocket API and adds
word timing events for the timed stream. The current deployment
uses Pocket exclusively in the native Listen player. Enabled Pocket
presets include the original Alba, Marius, Javert, Jean, Cosette, Eponine, Fantine,
and Azelma, plus Anna, Bill Boerst, Caro Davy, Charles, Eve, George, Jane, Mary,
Michael, Paul, Peter Yearsley, Stuart Bell, and Vera: 21 English voices in total.
All are supported by the pinned Pocket TTS 3.1.0 package; the model is unchanged.
No voice recordings or cloning setup are needed for these presets.

## Prepared voices (voice directory)

The worker loads prepared voice states from `POCKET_TTS_VOICE_DIR`. The
default is `/app/voices`, which the image bundles with the original nine states.
On VM310, both workers mount `/opt/docker/artifact-pocket-tts/voices` read-only at
`/voices`. To add a voice, copy a `.safetensors` state into that directory, add
an entry to its `manifest.json`, and restart the workers. No code change is needed.

`manifest.json` has a `results` list. The worker loads the listed voices in list
order and skips files that the manifest does not list. Each entry needs `voice`
(the file stem, `^[a-z][a-z0-9_]{0,39}$`). Optional fields:

- `label`: display name; the worker shows it as “Label · Pocket”.
- `embedding_sha256`: the worker skips the voice if the file hash differs.
- `cache_key: "legacy"`: keep the cache key without the content hash. The nine
  original voices use it so that their existing cached audio stays valid.
  Other voices include the file hash in the cache key, so a replaced state never
  reuses stale audio.

Other fields (source, license, consent, gains) are provenance and are not read by
the worker. Without a manifest, the worker loads every valid file with a
title-cased label. Names that match built-in presets, symlinks, and files larger
than 16 MiB are skipped and logged. A bad entry does not stop the worker.

The worker serves every loaded voice. Artifact MCP controls which voices users
see, through its built-in list, `POCKET_TTS_EXTRA_VOICES`, and the voice catalog
(`POCKET_TTS_VOICE_CATALOG`). The Voice-Zero states stay loadable but hidden.
[`voices/manifest.json`](voices/manifest.json) is the source copy for the nine
original voices. `Dockerfile.voice-dir` builds the incremental image layer that
the live workers use; see [voice-dir parity](trials/voice-dir-parity/README.md).

For cloned voices, keep written consent from the speaker that explicitly
permits creation and use of the clone. Public-domain recordings do not count as
speaker consent. Do not add a cloned voice without that explicit written
permission.

## Voice sources and credits

| Source | Licence | Catalog `source` | Audition |
|---|---|---|---|
| Kyutai Unmute voice donations | CC0-1.0, donor agreement covers voice transformation | `kyutai-donation` | [trials/kyutai-donations-20261018](trials/kyutai-donations-20261018/README.md) |
| CSTR VCTK Corpus 0.92 | CC BY 4.0, attribution required | `custom` (manifest: `VCTK (CC BY 4.0)`) | [trials/vctk-20261019](trials/vctk-20261019/README.md) |
| Voice-Zero | CC0 | `voice-zero` | [voices/README.md](voices/README.md) |

**VCTK credit.** Voices derived from the VCTK corpus, CSTR, University of
Edinburgh, CC BY 4.0. Corpus: Junichi Yamagishi, Christophe Veaux, and Kirsten
MacDonald, *CSTR VCTK Corpus: English Multi-speaker Corpus for CSTR Voice Cloning
Toolkit (version 0.92)*, University of Edinburgh, CSTR, 2019,
<https://doi.org/10.7488/ds/2645>. Licence:
<https://creativecommons.org/licenses/by/4.0/>. Changes: a 10–15 s reference clip
per speaker, made from 3–5 recordings and level-normalised, conditions Pocket TTS,
which then speaks new text. Keep this credit wherever VCTK voices are listed or
shipped. Each manifest entry carries `license`, `attribution`, and `source_doi`.
VCTK was recorded for speaker-adaptive speech synthesis and is CC BY 4.0, but we
hold no per-speaker written consent (see the rule above). The voices were
prepared at Neil's request, and keeping any is his decision.

## Adding voices: audition workflow

1. Export candidate states with the donation procedure (verify `andrea.safetensors`
   reproduces first), synthesize the two audition sentences, and run the analyzer.
   See the trial READMEs for the commands.
2. Select (`select_candidates.py` for donations, `select_vctk.py` for VCTK) to
   write `voice-metrics.json`. Build and publish the audition bundle with
   `build_audition.py` and `publish_bundle.py` (optional template, artifact key,
   and title arguments).
3. Stage the selected states into the worker voices directory with
   `stage_worker_voices.py` (keeps a `manifest.json.pre-*` rollback copy), restart
   both workers one at a time, and check `/health` and `/speech` on :8790 and :8794.
   Staged voices are loaded but not visible.
4. After Neil decides, build the catalog from every audition at once:

   ```sh
   python3 ops/pocket-tts/voice_catalog.py \
     --audition ops/pocket-tts/trials/kyutai-donations-20261018/voice-metrics.json \
     --audition ops/pocket-tts/trials/vctk-20261019/voice-metrics.json \
     --from-ct220 --worker-manifest ops/pocket-tts/voices/manifest.json --out /tmp/pocket-catalog
   ```

   `catalog/label-overrides.json` (`{voice id: label}`) is applied last, so a
   rename made outside an audition survives every regeneration. Check
   `summary.json` (kept, maybe, skipped per audition; unused overrides; duplicate
   labels), then install `pocket-voices.json` on CT220. artifact-mcp reloads it
   without a restart.

## Deployment

The tested host is VM310, directory `/opt/docker/artifact-pocket-tts`, with private
port 8790. The container has a two-core CPU quota, 3 GiB memory limit, non-root
user, read-only root filesystem, and persistent model/audio caches. It joins the
existing `artifact-tts-trial_default` Docker network. The first start downloads
public model assets; subsequent starts reuse `/models/huggingface`.

Copy this directory to the host, create `models`, `cache`, and `voices` (the
voice directory may be empty) owned by UID 65532,
and install a bearer token of at least 32 characters as `tts-token`, mode 0400,
owned by UID 65532. Set `TTS_BIND_HOST` in `.env` to the private interface address.
Run `docker compose up -d --build` and wait for the container health check.
The health endpoint reports available voice IDs without requiring the token.

Set `POCKET_TTS_WORKER_URL=http://192.168.0.110:8790` on artifact-mcp and restart it.
The server uses `TTS_WORKER_TOKEN_FILE` unless `POCKET_TTS_WORKER_TOKEN_FILE` is set.
Do not expose worker ports publicly. Browser requests use the authenticated
artifact server, which holds worker credentials and enforces artifact access.
Remove the Pocket URL and restart to remove these voices.

The worker accepts authenticated `POST /speech` with `{text, voice}`. It also
supports `POST /speech/stream`, returning the artifact framed PCM protocol so
playback can begin while Pocket is generating. Streaming caches only complete
responses and uses the same one-inference gate as WAV requests; interrupted or
truncated generations are never cached. Voice IDs
include the `pocket_` prefix. Limits: 1,500 characters, 8 KiB body, one concurrent
inference, and 512 MiB audio cache. The viewer prepares at most one upcoming chunk.
Stream output is mono 24 kHz PCM16, in frames of at most 9,600 bytes, with a terminal zero frame;
the original complete PCM16 WAV endpoint remains available.

`POST /speech/stream-timed` uses the versioned
`application/vnd.artifact.pcm-timed;v=1` protocol. Each HTTP chunk is a four-byte
big-endian length followed by a record whose first byte is `1` for PCM16 audio or
`2` for compact JSON word events. Word events include `word`, `index`, and
`start`; completed events also include `end`. A zero-length record terminates the
stream. Timed responses have a separate cache namespace from ordinary PCM.

## Measured comparison — September 12, 2026

Both engines used the same two-core limit on VM310. Two identical passages were
rendered uncached for each voice. See [raw measurements](benchmark.json).

Completed audio is served directly from the cache before the inference gate is
checked, so a replay can start while another passage is being generated. Cache
reads and bounded eviction are serialized and use atomic replacement, preserving
the shared 512 MiB limit without exposing partial files. Cache misses enter a
two-request FIFO wait queue for up to eight seconds; excess or expired waiters
receive the existing authenticated `429 busy` response. The worker emits
text-free JSON timing events for cache hits, queue wait, first audio, and total
generation time.

| Voice | 306-character passage | 574-character passage | Generation / audio duration |
|---|---:|---:|---:|
| Kokoro George | 7.22 s | 13.94 s | 0.392 / 0.392 |
| Pocket Alba | 5.38 s | 10.61 s | 0.354 / 0.366 |
| Pocket Marius | 5.48 s | 10.56 s | 0.358 / 0.399 |

Pocket returned these passages about 24–25% sooner, but also spoke them faster.
Both engines generated audio comfortably faster than playback. This small sample
establishes usability, not a general speed or voice-quality ranking. Compare the
voices in the same artifact; pronunciation and narrator preference need listening.

## Qwen follow-up

VM310 already has a stopped Qwen3-TTS stack using the 1.7B Base model and a CUDA
runtime. Its RTX 3090 was nearly full with another workload during inspection;
this test did not start Qwen or alter that workload. Qwen's CustomVoice variant
provides preset speakers (including English Ryan and Aiden); Base is for cloning.
The available AMD R9700 host needs separate runtime compatibility validation.

Sources: [Pocket TTS](https://github.com/kyutai-labs/pocket-tts),
[Qwen3-TTS](https://github.com/QwenLM/Qwen3-TTS).

## Expanded voice validation

All 21 voices generated valid non-silent, mono 24 kHz PCM16 WAV audio on VM310
with the existing two-core quota. For the same 46-character sentence, uncached
generation took 0.77–1.44 seconds and produced 2.08–3.76 seconds of speech. These
short-sample timings validate operation, not subjective quality or long-book
performance. See [per-voice results](expanded-voice-validation.json).

## Streaming lifecycle

The worker uses Pocket's default text-token chunking and sampling settings.
Listen groups complete sentences into requests of at most 600 Unicode characters
and prepares one upcoming request after the current generation finishes.

Cancellation and the 55-second generation deadline are checked at each emitted
latent through the pinned 3.1.0 `_autoregressive_generation` queue hook. The worker
drains the cancelled iterator through Pocket's decoder cleanup before releasing
its inference gate. A single PyTorch operation cannot be interrupted. A model
upgrade must recheck this private hook and cancellation behavior. Generations
that reach Pocket's maximum length without an end marker fail instead of being
cached as complete audio. WAV and PCM files share one 512 MiB cache budget.

The player remembers voice and speed in browser local storage. Preview paragraph
reads one paragraph, then restores the reading position without advancing chapters.

## Int8 quantization

The worker supports dynamic int8 quantization through `POCKET_TTS_QUANTIZE`. It is
enabled in the live deployment after the Alba listening comparison on September 12,
2026. The Compose fallback remains float32 for deployments without an explicit setting.
Set `POCKET_TTS_QUANTIZE=1` in the worker’s `.env` and rebuild/recreate with
`docker compose up -d --build` to apply it. Accepted values are `1`, `0`, `true`, `false`, `yes`, `no`,
`on`, and `off`. Invalid values fail startup so a deployment cannot silently use
an unintended model mode. Quantized and float workers use different model IDs and
cache namespaces, preventing audio generated in one mode from being reused by the
other. `/health` reports the active mode as `quantized`.

An isolated comparison on VM310 used the same pinned image, Pocket model, two-core
quota, 3 GiB memory limit, and 662-character synthetic prose passage for both
`alba` and `marius`:

| Mode | First audio | Generation RTF | Peak RSS | Valid output |
|---|---:|---:|---:|---|
| Float32 | 0.146–0.237 s | 0.353–0.450 | 982.5 MB | 4/4 trials |
| Dynamic int8 | 0.086–0.106 s | 0.241–0.256 | 982.6 MB | 4/4 trials |

The int8 trial was approximately 27–46% lower generation time per second of audio and started audio sooner. The
process-level RSS measurement did not show a reduction, so it should not be used
as a model-memory measurement. The host was shared with other work, and generated
audio is stochastic. The user found the Alba samples close in sound and approved
int8 as the live default after listening. See the [raw benchmark](quantization-benchmark.json).


The browser uses the vendored SoundTouchJS 2.1.1 AudioWorklet for pitch-preserving
speed at 0.8×, 1.25×, 1.5×, and 2×. Its 200 ms buffer spans network frames and is
included in saved-position accounting. Normal 1× playback bypasses this processor.
Pause suspends the audio clock; seek and speed changes reset processor history.
Browsers without AudioWorklet support use complete WAV playback at non-1× speeds,
with the browser's native pitch preservation. This fallback waits for generation.
See [vendor source and licensing](../../assets/vendor/README.md).


### Rollback to float32

Set `POCKET_TTS_QUANTIZE=0` in the worker’s `.env`, then run
`docker compose up -d --no-build speech`. The original float32 cache namespace is
preserved. The live rollout also retained the previous image and source/configuration
backup at `/opt/docker/artifact-pocket-tts/.pre-int8-20260912` on the Docker host.

## Word highlighting deployment

The September 12, 2026 deployment uses
`homelab/artifact-pocket:3.1.0-timestamped-1`, with fork commit
`65037e84c1885e7faa3e482b89fe3c304e2dada2` and int8 enabled. The deployed image was
built incrementally from the verified prior CPU image, copying the pinned fork
and worker files. The Dockerfile here supports rebuilding from the pinned source
archive. Every audio cache namespace includes the fork revision.

The viewer requests `/speech/stream-timed` for Pocket, with legacy streaming
fallback when that endpoint is unavailable. The selected passage remains marked
while audio prepares. Word events are mapped to normalized source-text offsets
and rendered through the CSS Highlight API, preserving the artifact DOM. Word
highlighting follows the played audio position, including speed changes and the
pitch processor's latency. Pause holds the current word; Stop and completion
clear it. Unsupported browsers and failed text mapping retain passage highlighting.
Complete WAV fallback does not supply word timings.

Validation covered real streamed audio, exact cache replay, legacy PCM/WAV,
disconnect recovery, nested formatting, paragraph boundaries, line breaks,
selection offsets beyond the first chunk, and clearing highlights on mutation.
The browser mapping regression is `playwright/tests/reader-word-highlights.cjs`.
See [the initial trial](trials/timestamped/README.md) and [the next RAVEN trial](trials/raven/README.md).

The worker backup is `/opt/docker/artifact-pocket-tts/.pre-timestamped-20260912`.
To restore the previous worker, restore its `compose.yml`, `Dockerfile`,
`server.py`, and `stream_control.py` from that directory, then recreate `speech`
with `docker compose up -d --no-build speech`. The updated viewer falls back to
ordinary streaming if the old worker does not expose the timed endpoint.
The native Artifact MCP binary backup is
`/usr/local/bin/artifact-mcp.pre-pocket-word-highlights-20260912` on CT220.

## Queue/cache deployment

The queue and cache changes were built and tested in the isolated image
`homelab/artifact-pocket:3.1.0-cache-queue-20260913`, digest
`sha256:4c5c3ea88aa3b2b684839a2f11ccf968503640a7f46f95521290a0c29bf16fa0`.
The candidate passed health checks and a real VM310 smoke test: timed generation
returned HTTP 200 with 88,835 bytes, a complete timed cache replay returned HTTP
200 with 88,835 bytes in 3 ms, and an uncached concurrent stream also completed
successfully while the replay was served. The candidate container was stopped
after testing. Following explicit approval, production switched to this image on
2026-09-14 and passed its health check. The previous image was
`3.1.0-timestamped-1`, digest
`sha256:d26ef8020fc5b8c57e91df985c0282d0cbde7cfb8089275711f728597ea45b55`.

The prepared [cutover script](deploy-cache-queue.py) runs on VM310. It checks the
running production image and tested candidate digest, creates a timestamped compose
backup, and recreates only `speech`. It restores the compose file and service if
recreation fails or health does not recover within 180 seconds. The deployment backup is
`/opt/docker/artifact-pocket-tts/compose.yml.pre-cache-queue-20260914-000445`.
Restore that compose file and run `docker compose up -d --no-build speech` from
the stack directory to roll back.

Production verification returned complete timed audio with word events. A cached
150,945-byte response matched the initial response exactly and completed in 4 ms
while another generation was active. That concurrent generation also completed
successfully. These are worker-local smoke measurements, not browser latency.
