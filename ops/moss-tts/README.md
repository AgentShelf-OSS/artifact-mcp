# MOSS-TTS-Nano ONNX CPU worker

This worker adds the upstream MOSS-TTS-Nano 100M ONNX CPU backend to artifact
narration. It uses the upstream built-in voice prompts, keeps one inference in
flight, and returns normalized mono 24 kHz PCM16 WAV so it fits the artifact
server's existing audio contract. The model itself generates native 48 kHz
stereo audio; the worker downmixes and resamples it with SciPy’s anti-aliasing polyphase filter.

The upstream commit used for this trial is
`8b7bcc9341b3b4ef3a3a58ba1338a7d85ff133eb`. It has 18 bundled prompts. The
English voices exposed by this worker are:

`moss_trump`, `moss_ava`, `moss_bella`, `moss_adam`, and `moss_nathan`.

The two ONNX repositories are downloaded by the upstream runtime into
`models/` (mounted at the runtime's `/app/vendor/models`, including its
Hugging Face cache) on first start:
`OpenMOSS-Team/MOSS-TTS-Nano-100M-ONNX` at revision
`f52645cb467506d8e18e746ddd59482685b74e58` and
`OpenMOSS-Team/MOSS-Audio-Tokenizer-Nano-ONNX` at revision
`ceff0d0749bfb3fa2d61149794ec6feef0d1e1ae`.

The worker accepts authenticated `POST /speech` with `{text, voice}`. Text is
limited to 1,500 characters and 8 KiB JSON bodies; audio cache is capped at 512
MiB. A 55-second deadline is checked between generated frames, and reaching
the per-chunk frame limit is rejected rather than returned as truncated speech.
A single ONNX operation is not interruptible. `GET /health` reports voices and model-load time without authentication.
Reference audio cloning is deliberately not exposed in this initial worker.
Do not publish port 8792 beyond the private host interface.

## Deployment

Create `models` and `cache` owned by UID 65532. The Compose file reuses
`../artifact-tts-trial/tts-token`, which must be readable by that UID. Set `TTS_BIND_HOST` to the private VM310 address, then run:

```sh
docker compose up -d --build
```

Set `MOSS_TTS_WORKER_URL=http://192.168.0.110:8792` in artifact-mcp after the worker is healthy. The first startup downloads the ONNX assets and can take
several minutes; later starts reuse `models/`.

Source: [OpenMOSS/MOSS-TTS-Nano](https://github.com/OpenMOSS/MOSS-TTS-Nano).

The vendored runtime omits unused PyTorch reference-audio imports, pins both
model downloads, resolves its writable model directory inside the container,
and disables idle ONNX thread spinning to avoid exhausting the CPU quota.

Listen uses 300-character MOSS chunks to reduce startup waits and leave
headroom under the proxy deadline. Other engines retain their existing chunk size.
