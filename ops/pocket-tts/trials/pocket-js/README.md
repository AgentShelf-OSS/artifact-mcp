# pocket-tts-js browser trial

Tested 2026-09-13 using the upstream HTTPS demo:
https://vlapky.github.io/pocket-tts-js/

Source reviewed at commit `7d7a27423b0845eb0425c81a8aa5ed3f3d973eef`:
https://github.com/vlapky/pocket-tts-js
The hosted demo and its model URLs are mutable, so this measurement is not a pinned-build benchmark.

## Try it

1. Open the demo in a desktop browser.
2. Keep English and Quantized INT8. Uncheck Voice cloning before loading.
3. Click Load model, select Alba or Marius, then Load built-in voice.
4. Paste a short passage and click Generate.

The initial preset-only model and voices download is approximately 178 MB, plus runtime assets. The upstream demo caches model assets in browser Cache Storage by default. This differs from Artifact MCP's temporary playback cache. Clear this site's browser data to remove it.

## Observed run

Headless Chromium on an AMD Ryzen 7 5700X host, upstream demo reporting `crossOriginIsolated = false`, therefore single-thread WASM. English INT8, Alba, cloning disabled. One first-generation sample, no statistical performance comparison.

- Core model ready: 6.8 seconds, excluding the later voice load.
- First generated audio chunk: 584 ms, not measured audible onset.
- Audio duration: 7.28 seconds.
- Generation: 10.19 seconds, 0.71 times real-time.
- Six playback underruns, reported gaps of 192–661 ms.
- Eight available presets: alba, azelma, cosette, eponine, fantine, javert, jean, marius.

See browser-result.txt for the exact text and log. Playback controls and completed generation were verified in a browser screenshot. Audio quality was not subjectively assessed.

## Integration implications

This implementation uses WASM CPU inference, not WebGPU. Cross-origin isolation enables multiple WASM threads; this run did not exercise that mode. Device and browser performance can differ substantially.

It exposes audio chunks but no word timestamp events, so our existing word-following behavior would need additional work. Artifact HTML currently blocks outbound connections and runs in an isolated sandbox. A future browser provider belongs in a reviewed native player integration or separate trusted page, not a relaxation of ordinary artifact CSP.

Production Pocket remains unchanged. No container is needed for the upstream browser demo. This trial is for assessing device-side generation before deciding whether to build a native option.
