# Qwen3-TTS streaming adapter

The deployed model is Qwen/Qwen3-TTS-12Hz-1.7B-CustomVoice on VM310's RTX 3090.
The native menu offers Ryan and Aiden. Neil selected preset voices only, so the
previous Base reference voice is disabled. No second model or decoder runtime
patch is deployed. The original Base Compose is backed up on VM310 as
`/opt/docker/qwen3-tts/docker-compose.pre-stream-presets.yml`.

## Configuration

The adapter at `/opt/docker/artifact-qwen-tts` joins `navi_voice`, resolves
`qwen3-tts:8000`, and binds authenticated requests to private port 8791. Configure
`.env` with `TTS_BIND_HOST`, `QWEN_CUSTOM_URL=http://qwen3-tts:8000`,
`QWEN_REFERENCE_ENABLED=0`, and `REFERENCE_SHA256` for cache-version compatibility.
Install `tts-token` mode 0400, owned by UID 65532; create `cache` with the same owner.
Run `docker compose up -d --build`.

On artifact-mcp, set `QWEN_TTS_WORKER_URL=http://192.168.0.110:8791`,
`QWEN_TTS_CUSTOM_VOICES_ENABLED=1`, and `QWEN_TTS_REFERENCE_VOICE_ENABLED=0`.
`QWEN_TTS_WORKER_TOKEN_FILE` defaults to `TTS_WORKER_TOKEN_FILE`.
Restart artifact-mcp after changes. Kokoro remains the initial default.

## Progressive playback

`POST /{id}/speech/stream` accepts `{text, voice}` plus an optional `instructions`
string. Instructions are trimmed, limited to 500 Unicode characters, and reject
Unicode control characters. For CustomVoice presets, vLLM maps this
OpenAI-compatible field to Qwen's native `instruct` style control. Empty or
omitted instructions preserve the existing model-default behavior. Instructions
are part of the cache key, so changing delivery never reuses audio generated
with another style.
It enforces artifact authorization and mutation checks before contacting the worker.
Qwen emits raw PCM during generation; the adapter forwards it without waiting for
the whole passage. Rust and Node preserve streaming and cancel upstream work when
the browser disconnects.

The body type is `application/vnd.artifact.pcm`: unsigned 32-bit big-endian frame
length, followed by that many PCM bytes, repeated. Audio is signed 16-bit
little-endian, mono, 24 kHz. A zero-length frame marks successful completion.
Missing termination, odd PCM lengths, oversized frames, or oversized totals stop
playback. Frames are limited to 192 KiB and audio to 4 MiB.

The browser schedules Web Audio buffers with a 200 ms initial startup cushion.
Once generation finishes, it requests one upcoming paragraph while the current
audio plays. That response retains browser/network backpressure until consumed;
only the current paragraph and one upcoming response are retained. A prefetched
paragraph uses a 10 ms startup cushion. Stop aborts both requests. Chapter changes
still extract the next chapter before requesting its audio. Pause
suspends its audio clock; resume preserves position. Rate changes reschedule the
unplayed samples. Stop, voice changes, and navigation cancel the stream and all
scheduled audio. Reading proceeds to the next paragraph and supported chapter.
Kokoro/Pocket keep their WAV path. Qwen falls back to WAV if Web Audio or the
streaming route is unavailable; an interrupted stream stops with an error.

Adapter limits: one concurrent request, 8 KiB request body, 1,500 text characters,
55-second upstream deadline, 512 MiB disk cache. Complete streams are cached;
partial streams are discarded. Worker credentials and model/reference paths
remain server-side. The adapter has a read-only root, non-root user, 128 MiB RAM,
and a half-core CPU quota. GPU residency belongs to the Qwen model container.

## Restore the language-model workload

First disable Qwen voices in artifact-mcp by unsetting its worker URL and restarting.
On VM310:

```sh
docker compose -f /opt/docker/qwen3-tts/docker-compose.yml stop qwen3-tts
docker compose -f /opt/gpu-serving/compose.vm310.yaml start runtime
```

Pocket and Kokoro remain on CPU; no automatic GPU switching occurs.

## Paragraph prefetch regression check

After building the native release and installing `playwright/` dependencies, run:

```sh
node playwright/tests/reader-prefetch.cjs
```

Set `STREAM_TEST_BINARY` to use a binary built outside this worktree. The test uses
synthetic PCM and delayed delivery to verify that the next paragraph is requested
during playback, the prepared transition stays below 100 ms, lookahead remains
bounded, and Stop aborts outstanding work. It does not measure model-generated
silence or GPU synthesis speed.

## Reader controls

Selecting Ryan or Aiden reveals **Reading style** in the native Listen player.
Calm audiobook is the initial choice; Neutral, Expressive, Model default, and
Custom instructions are also available. Model default sends no instruction.
Custom instructions are limited to 500 characters. The style and custom text
are remembered in this browser. Other voices do not expose these controls.

Changing the style stops playback and cancels prefetched audio while keeping the
current reading position. **Preview paragraph** reads the current block, including
its text chunks, then stops without advancing to another paragraph or chapter.
Repeating Preview uses the same block; Play resumes from the saved position.
A style guides the model's delivery, so it does not guarantee a particular sound.
