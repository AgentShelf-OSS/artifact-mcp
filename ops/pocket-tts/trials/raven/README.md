# PocketTTS-RAVEN comparison

Built and tested September 12, 2026, America/Denver. This is an isolated CPU trial. Production remains on the timestamped Pocket int8 worker.

[Listen to the comparison](https://artifact.neilblackman.dev/9ayvykkd2zuh), revision 1. It includes current Pocket and RAVEN samples for Alba prose, Marius prose, and Alba numbers/abbreviations, plus three RAVEN startup variants. All clips are prerecorded. Playback does not reproduce generation delays, and no listening-quality preference has been inferred from the measurements.

## Setup

- Host: VM310, `/opt/docker/artifact-raven-trial`.
- RAVEN source: https://github.com/pkalogiros/pocket-tts-raven at `abd26158ab50f954616eaf42296b09c4856489d7`.
- Image: `homelab/artifact-raven:abd26158-trial-1`, inspected digest `sha256:a06f8db19bf13fae3a02b543f1422088810e7e3b5d8ba993f0887808c8977e16`.
- English April 2026 ONNX bundle, hash-verified by upstream preparation. Graph rewrites and included equivalence checks completed. See `model-sha256.txt`.
- ONNX Runtime 1.23.2, CMake 3.31.6, ONNX 1.20.1, NumPy 2.4.6. Build uses two compiler jobs.
- Inference: two CPU quota, 3 GiB memory cap, no GPU, no network, read-only root. RAVEN thread budget 2 splits into one autoregressive thread and one decoder thread.
- Preparation has outbound access through the existing TTS trial network and a 4 GiB cap. It publishes no ports.
- Models, Alba/Marius voice caches, scripts and results are retained. The benchmark containers exit after each run. The optional `raven` service keeps RAVEN resident for the native Listen player trial.

The Dockerfile corrects upstream's shallow checkout of its pinned `dr_libs` revision. It preserves the dependency pin and fetches history so Git can resolve it. Model preparation uses the pinned Python packages installed in the image instead of resolving a fresh uv environment for every graph rewrite.

## Voices and settings

Only the official Kyutai Alba and Marius reference recordings were used. Their repository revision and SHA-256 values are recorded in `voice-manifest.json`; `fetch_voices.py` fetches that pinned revision.

The native C++ runtime cannot directly load the current Python Pocket preset-state files. It encodes the official recordings into its own embedding and transformer-state caches. This preserves the preset source but is not identical voice conditioning to the Python runtime. The other 19 production voices have not been validated in RAVEN.

Both engines used int8 and temperature **0.3**, matching our production checkpoint rather than RAVEN's 0.7 default. RAVEN used one flow step and retained commas. The resident-engine measurements use RAVEN's standard startup gate. CLI low-latency settings are a separate comparison.

## Measurements

Medians of three warm runs for each short case, with no audio-response cache. Model initialization and initial voice conditioning are excluded. The engines ran sequentially on the same shared host; the benchmark is exploratory, not a controlled hardware study. Audio lengths differ because synthesis is stochastic and the implementations differ.

| Case | Pocket first audio | RAVEN first audio | Pocket generation | RAVEN generation |
| --- | ---: | ---: | ---: | ---: |
| Alba prose | 97 ms | 91 ms | 2.91 s | 1.75 s |
| Marius prose | 102 ms | 114 ms | 2.57 s | 1.77 s |
| Alba numbers/abbreviations | 106 ms | 160 ms | 4.62 s | 3.40 s |

RAVEN used 26–40% less total generation time in these short cases. Its median generation speed ranged from 4.6× to 6.3× real time, versus 3.3× to 3.5× for timestamped Pocket. It did not consistently improve first-audio time.

A single long-text run generated 44.5 seconds of RAVEN audio in 7.82 seconds; Pocket took 14.53 seconds. Treat that as a smoke test, not a repeated performance result.

The first uncached voice generation took 2.64 seconds to first audio for Alba and 1.88 seconds for Marius. Subsequent runs used the retained caches. Cancellation after the first chunk completed in 282 ms for RAVEN and 3 ms for Pocket; subsequent generation succeeded for both. RAVEN's maximum process RSS was about 1.3 GiB, including voice encoding and allocator retention.

The three CLI startup samples include process and model loading. First audio was 588 ms with the default gate, 539 ms with low latency and the gate disabled, and 577 ms with low latency and the gate retained. These are single runs and are not comparable to the resident-engine timings above. Their main purpose is listening for leading blips or clipped opening words.

Raw evidence: `raven-results.json`, `pocket-results.json`, `cli-results.json`. The Pocket startup figure includes Python imports, model load and preset load; do not compare it directly to RAVEN model-only initialization.

## Reproduce

On VM310, the existing directory is ready:

```sh
cd /opt/docker/artifact-raven-trial
docker compose run --rm trial >raven-benchmark.log 2>&1
docker compose run --rm baseline >pocket-benchmark.log 2>&1
docker compose run --rm -v "$PWD/cli_variants.py:/trial/cli_variants.py:ro" trial python /trial/cli_variants.py >cli-benchmark.log 2>&1
```

For a fresh build, copy the files in this directory into a scratch directory on VM310, clone the pinned upstream source into `source/`, and create writable `models`, `voices`, and `results` directories owned by UID 65532. The baseline Compose service assumes this homelab's existing model directory and timestamped Pocket image.

```sh
git clone https://github.com/pkalogiros/pocket-tts-raven.git source
git -C source checkout abd26158ab50f954616eaf42296b09c4856489d7
python3 fetch_voices.py
docker compose config --quiet
docker compose build prepare
docker compose run --rm prepare >prepare.log 2>&1
```

After retrieving the result audio locally:

```sh
python3 build_demo.py /path/to/results /path/to/demo
```

Browser validation covered all six engine-comparison clips, all three startup variants, exclusive playback, pitch-preserving speed controls, desktop and mobile layouts, and no page/console errors under artifact-style sandbox/CSP restrictions.

## Decision still open

RAVEN merits a listening comparison because generation is faster on our hardware. It currently exposes no word timestamps. Switching the reader to it would lose the new word highlighting unless we add a compatible alignment path. Pocket remains the default. RAVEN is an optional Listen player trial with passage highlighting. This image includes build tools and is intended for trials, not as a finished production worker.


## Native Listen player trial

The optional `raven` service uses the same resident native engine and official Alba/Marius caches as the comparison. It exposes authenticated WAV and framed PCM endpoints, with one generation at a time. It has no word timestamp endpoint.

Set `RAVEN_BIND_ADDRESS` to the private worker address in the stack `.env`, provide the existing worker credential as `tts-token`, then start only this service:

```sh
docker compose config --quiet
docker compose up -d --no-build raven
```

On Artifact MCP, set `RAVEN_TTS_WORKER_URL` to the private worker URL on port 8795 and `RAVEN_TTS_WORKER_TOKEN_FILE` to the credential file. The generic `TTS_WORKER_TOKEN_FILE` is also accepted as a fallback. Keep `POCKET_TTS_WORKER_URL` configured. The voice picker adds **Alba (trial)** and **Marius (trial)** in a **RAVEN trial** group after Pocket's voices. Existing saved voice choices remain in effect.

RAVEN uses the native player's sentence splitting, complete paragraph WAV playback, next-paragraph prefetch, browser pitch-preserving speed controls, pause/resume, Read from Here, and cooperative chapter continuation. Live chunk playback is temporarily disabled for RAVEN after a buzzing report. The first paragraph must finish generating before playback starts. It retains passage highlighting during playback. Choose a Pocket voice to use word highlighting again.

To retire the trial, remove the RAVEN environment settings and restart Artifact MCP, then run `docker compose stop raven` in this stack. The benchmark files and caches remain available.


### Player rollout validation, September 13, 2026

The live native viewer exposes Pocket and RAVEN. Its binary SHA-256 is `dd2885daf748e03a5ec943d996a91bd21f19432a8abddfb15d708dd9f49ba7da`. The prior binary is retained on CT220 as `/usr/local/bin/artifact-mcp.pre-raven-player-20260913`; the optional provider settings are in `zzz-raven-trial.conf` in the service drop-in directory. The 1984 artifact itself was not edited.

Checks passed: 17 Node proxy tests, 5 Rust speech tests, 7 worker protocol/cleanup tests, and the existing pitch-processing test. Browser tests used the release binary with the real private workers. They covered RAVEN Alba chapter continuation in the 1984 HTML, Marius Read from Here and selected-text playback, passage retention, pause/resume, 1.5×/2× speed, stop cleanup, and switching to Pocket word highlighting. Desktop and mobile screenshots were inspected, with no page errors or mobile overflow. These browser checks ran on a local authenticated fixture; the public Cloudflare login was not automated.

This HTTP trial does not cache completed audio responses. It retains the native model and preset conditioning caches; paragraph prefetch comes from the shared player.


### Buzzing report, September 13, 2026

The user reported buzzing with both live trial voices. The cause is unresolved; the earlier functional playback checks did not establish audible quality. A [direct-audio diagnostic](https://artifact.neilblackman.dev/4h5mmdez3cye) contains fresh Alba and Marius recordings captured from the live worker's framed PCM endpoint and assembled into WAV without audio processing. These bypass the native player's streaming and speed processing, and are not the exact generations the user heard.

Captured frames passed length, parity, and terminator checks. Neither recording clipped. Rendering the same captured PCM as separate frames versus one continuous buffer in Chromium at 48 kHz produced signal-to-difference ratios of 55.8 dB for Alba and 69.2 dB for Marius. That measures boundary differences, not perceived buzzing, and does not establish a cause. No runtime settings were changed in response to this report. Awaiting the affected passage/speed or confirmation that the direct clips contain the same noise.


### Buffered playback workaround

The user confirmed that both direct diagnostic recordings sounded fine. The viewer now plays RAVEN as complete paragraph WAVs through the browser audio element, bypassing per-chunk Web Audio scheduling and its speed processor. Next-paragraph prefetch remains enabled. Both voice listings advertise `streaming: false`; the worker and authenticated PCM endpoint remain available for diagnosis. Pocket keeps timestamped streaming. This is a workaround, not a confirmed explanation or fix for the streaming noise.

Validation covered Marius selected text, Read from Here, pause/resume and 1.5× playback, with requests asserted to use `/speech` instead of `/speech/stream`. Switching back to Pocket restored word highlighting. Node proxy tests passed, 17 total.


RAVEN was parked at Neil's request on September 13, 2026. Its provider drop-in was moved out of the systemd service directory to `/etc/artifact-mcp/raven-trial-parked-20260913.conf`, and the `raven` Compose service was stopped. Source, models and diagnostic recordings remain for later investigation. Pocket is the only enabled live provider.
