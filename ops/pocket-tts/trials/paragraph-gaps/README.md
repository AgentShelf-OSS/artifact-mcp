# Paragraph transition measurement

Measured on 2026-09-14 with a disposable native viewer in headless Chromium,
using the production VM310 Pocket CPU worker over the LAN. Voice: Alba. Four
short prose paragraphs, with one upcoming request prefetched. Production
artifacts were not modified. Browser measurement arrays contain no narration text.

| Configuration | First scheduled audio | Estimated inter-paragraph gap | Detected input underruns |
| --- | --- | --- | --- |
| 200 ms reservoir, initial 1x | 1919 ms | 11–12 ms | 0 |
| 200 ms reservoir, cached 1x | 217 ms | 11–12 ms | 0 |
| 200 ms reservoir, cached 1.5x | 422 ms | 211–212 ms | 0 |
| 120 ms reservoir, cached 1.5x | 345 ms | 131–132 ms | 0 |
| 120 ms reservoir, cached 2x | 338 ms | 131–132 ms | 0 |
| 120 ms reservoir, cached 0.8x | 418 ms | 131–133 ms | 0 |

These are scheduling estimates, not acoustic loopback measurements. They exclude
natural silence generated inside each narration chunk and do not cover the public
Cloudflare route, mobile devices, or contention from many simultaneous listeners.
The measured 80 ms reduction is about 38% of the previous added transition delay
at 1.5x. Normal-speed playback bypasses the pitch processor and is unchanged.

The processor remains separate per paragraph. Reusing it alone would not fix the
handoff because the next paragraph is scheduled after the previous output finishes.
A future larger change could decode one upcoming chunk and schedule the two audio
pipelines to meet, with separate replay, cancellation, and highlighting tests.

120 ms was selected as a conservative tested buffer, not as a proven minimum.
The actual SoundTouch processor passed 12 pitch/amplitude/tail tests across
0.8x, 1.25x, 1.5x, and 2x at 24, 44.1, and 48 kHz. Native browser tests passed
pronunciation highlighting, sentence replay, pause, selection, scope changes,
and mobile player behavior; pronunciation/replay was also checked at 1.5x.
The shell and processor must use the same delay for highlighting and saved position.

Raw measurements: [before](before.json), [after](after.json).
