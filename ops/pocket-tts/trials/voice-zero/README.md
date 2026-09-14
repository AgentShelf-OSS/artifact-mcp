# Voice-Zero Pocket TTS audition

This isolated trial prepares six CC0 Voice-Zero references for Pocket TTS
English 2026-04 int8 generation. New-voice synthesis is blocked on gated model
access; only the Alba control has been generated. These are trial assets, not
production presets.

Generated samples use the sentence:

> Winston Smith, alone in the room, listened to the faint mechanical hum.

Source references are pinned in `sources.tsv`. The labels describe the source
catalogue's accent notes where available; they are not a quality ranking.

`voice-zero-source-preview.html` is a self-contained preview gallery of the
six original source recordings. It labels them as source audio so they are not
mistaken for Pocket-generated output. `generate.py` is ready to rerun in an
isolated container once the gated Pocket voice-cloning weights are available.

Published source preview: https://artifact.neilblackman.dev/3hnzxgqixkbg

Browser validation passed source-audio playback, seven controls, exclusive playback,
mobile width, and no page errors. The original recordings use different passages;
this page is not a same-text comparison of generated voices.
