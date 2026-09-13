# Pocket TTS comparison worker

Pocket TTS 3.1.0 runs the `english_2026-04` model on CPU beside Kokoro. The native
viewer groups voices by engine; Kokoro remains the initial default. Enabled Pocket
presets include the original Alba, Marius, Javert, Jean, Cosette, Eponine, Fantine,
and Azelma, plus Anna, Bill Boerst, Caro Davy, Charles, Eve, George, Jane, Mary,
Michael, Paul, Peter Yearsley, Stuart Bell, and Vera: 21 English voices in total.
All are supported by the pinned Pocket TTS 3.1.0 package; the model is unchanged.
No voice recordings or cloning setup are needed for these presets.

## Deployment

The tested host is VM310, directory `/opt/docker/artifact-pocket-tts`, with private
port 8790. The container has a two-core CPU quota, 3 GiB memory limit, non-root
user, read-only root filesystem, and persistent model/audio caches. It joins the
existing `artifact-tts-trial_default` Docker network. The first start downloads
public model assets; subsequent starts reuse `/models/huggingface`.

Copy this directory to the host, create `models` and `cache` owned by UID 65532,
and install a bearer token of at least 32 characters as `tts-token`, mode 0400,
owned by UID 65532. Set `TTS_BIND_HOST` in `.env` to the private interface address.
Run `docker compose up -d --build` and wait for the container health check.
The health endpoint reports available voice IDs without requiring the token.

Set `POCKET_TTS_WORKER_URL=http://192.168.0.110:8790` on artifact-mcp and restart it.
The server uses `TTS_WORKER_TOKEN_FILE` unless `POCKET_TTS_WORKER_TOKEN_FILE` is set.
Do not expose worker ports publicly. Browser requests use the authenticated
artifact server, which holds worker credentials and enforces artifact access.
Remove the Pocket URL and restart to remove these voices without affecting Kokoro.

The worker accepts authenticated `POST /speech` with `{text, voice}`. It also
supports `POST /speech/stream`, returning the artifact framed PCM protocol so
playback can begin while Pocket is generating. Streaming caches only complete
responses and uses the same one-inference gate as WAV requests; interrupted or
truncated generations are never cached. Voice IDs
include the `pocket_` prefix. Limits: 1,500 characters, 8 KiB body, one concurrent
inference, and 512 MiB audio cache. The viewer prepares at most one upcoming chunk.
Stream output is mono 24 kHz PCM16, in frames of at most 9,600 bytes, with a terminal zero frame;
the original complete PCM16 WAV endpoint remains available.

## Measured comparison — September 12, 2026

Both engines used the same two-core limit on VM310. Two identical passages were
rendered uncached for each voice. See [raw measurements](benchmark.json).

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
