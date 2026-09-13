# Pocket Timestamped trial

Isolated CPU trial of [dpm63/pocket-tts-timestamped](https://github.com/dpm63/pocket-tts-timestamped), commit `65037e84c1885e7faa3e482b89fe3c304e2dada2`, run September 12, 2026, America/Denver.

Listening demo: https://artifact.neilblackman.dev/th7kbesekyx2, revision 1. Includes Alba and Marius prose, Alba numbers/abbreviations, and the current Alba baseline. Audio is prerecorded; generation performance comes from the measurements below. The production worker and player were not changed.

## Environment and reproduction

- VM310, same existing `homelab/artifact-pocket:3.1.0-stream-2` CPU image as production. Observed image digest `sha256:34e818d30b9f2387cf69c82c87146edc4b0400e688495604bf96487905e55400`.
- Torch `2.8.0+cpu`, Pocket `3.1.0`, checkpoint `english_2026-04`, one Torch thread, two CPU quota, 3 GiB memory cap.
- Fork source mounted via `PYTHONPATH`; no package upgrades, no network, no ports, existing models mounted read-only. Results directory writable by UID 65532.
- Scratch Compose project on VM310: `/tmp/pocket-timestamped-trial`. Containers automatically removed after completion.

Copy `compose.yml`, `trial.py`, `repeat.py` into a scratch directory on VM310. Clone the fork into `fork/` and check out the exact commit above. Create `results/` owned by UID 65532. The Compose model path is specific to this homelab.

```sh
docker compose config --quiet
docker compose run --rm trial >trial.log 2>&1
docker compose run --rm --entrypoint python -v "$PWD/repeat.py:/trial/repeat.py:ro" trial /trial/repeat.py >repeat.log 2>&1
```

The host emits repeated NNPACK unsupported-hardware warnings; retain stderr in a file rather than terminal output. Both baseline and fork use the same runtime. Timing results are exploratory measurements on a shared host, not a controlled hardware benchmark.

`trial.py` compares baseline int8, timestamped int8, and timestamped float32 across four voice/text cases each. It saves lossless FLAC and `results.json`. `repeat.py` measures three warm Alba prose runs for baseline, fork without timestamps, and fork with timestamps; it saves `repeats.json`. Generation time excludes model/voice loading. First-audio time is Python iterator latency, excluding HTTP, browser buffering and playback.

Build the self-contained demo locally after retrieving the audio files:

```sh
python3 build_demo.py /path/to/results /path/to/demo.html
```

## Results

All 21 production preset states loaded in all three configurations. Actual narration was tested with Alba and Marius. The eight timestamped runs produced 458 completed word units with contiguous indices, monotonic starts, nonzero durations, and timings inside the generated audio. Long input crossed internal sentence chunks successfully. This validates structure and coverage of returned units, not acoustic alignment against human labels.

Three-repeat medians, identical Alba prose, 10.32 seconds of generated audio:

| Mode | First audio | Total generation |
| --- | ---: | ---: |
| Current Pocket int8 | 76.4 ms | 2.575 s |
| Fork int8, ordinary stream | 78.6 ms | 2.826 s |
| Fork int8, timestamped stream | 83.1 ms | 2.806 s |

Timestamped generation was about 9% slower than the current baseline in this small repeat set. The plain and timestamped fork results overlap; do not attribute all overhead to attention capture. Single runs in the wider matrix were noisier, including slower Marius and mixed-text cases.

Closing the timestamped generator after the first audio chunk took 16.9 ms with int8 and 7.5 ms with float32. Neither left additional Python threads running, and subsequent narration completed. Word-start events arrived no later than the corresponding already-emitted audio position in these samples.

The demo passed Chromium checks at 1200×850 and 390×844: all four samples, word seeking, active highlighting, pitch-preserving playback at 0.8/1/1.25/1.5/2×, no horizontal overflow, and no console or page errors. This checks the demo's audio-clock mapping; it does not establish perceptual timestamp accuracy or validate production AudioWorklet integration.

## Integration recommendation

Proceed to an optional word-highlighting implementation if the listening demo feels aligned. Retain paragraph highlighting as fallback. Important work before production:

1. Carry audio plus timestamp metadata through a versioned endpoint/protocol. The existing worker and browser stream parser assume every frame is raw PCM; word events cannot be inserted into that stream unchanged.
2. Cache audio and word metadata together under a model/fork/protocol-specific key, including complete-audio responses and prefetch.
3. Update the worker's private `_autoregressive_generation` budget wrapper to forward the fork's `attention_capture` and `cancel_event` keywords. The current four-argument wrapper is incompatible with timestamped calls. Verify deadline, disconnect, and cancellation behavior through the HTTP path.
4. Map returned units to source text without changing artifact DOM semantics. For example `12.5%` becomes timed units `12` and `5`, and `$1,240` becomes `1` and `240`. Preserve punctuation and skip unreliable matches. The demo uses ordered literal matching; production needs a stronger mapping contract and paragraph fallback.
5. Drive highlighting from the actual played media position, including seek offsets and the existing pitch-worklet latency. Network arrival time is not playback time.
6. Check real reader behavior across speed changes, paragraph/chapter transitions, cached playback, pause/resume and navigation. Add an explicit opt-out if word motion is distracting.

No live backend switch, artifact-player changes, Git commit, or push was performed for this trial.
