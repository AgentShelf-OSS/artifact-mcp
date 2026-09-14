# Additional ungated Pocket presets

Audition: https://artifact.neilblackman.dev/zv7dr7u67d64

Generated 2026-09-14 in an isolated Compose job using the existing production image
`homelab/artifact-pocket:3.1.0-cache-queue-20260913`. Model mounts were read-only;
no production voices or worker configuration changed. The job was removed afterward.

Estelle, Giovanni, Juergen, Lola, Rafael and baseline Alba use public precomputed
English voice states at revision e81d79e8194ad4c7ce879c87a4258ef20cbf2487.
No gated model or Hugging Face credentials were needed. All samples use the same
passage, timestamped English 2026-04 int8 model, one Torch thread, and copy_state.

`results.json` records durations, levels and WAV checksums. All outputs were finite
24kHz mono audio with zero samples exceeding full scale. Browser validation passed
six controls, playback, exclusive playback, and mobile layout with no page errors.
This is a listening audition, not a transcription-accuracy or long-form stability test.

`generate.py` expects /trial and cached model weights. `build_gallery.py` needs
ffmpeg and embeds 96kbps MP3 previews in the standalone HTML. WAV originals remain
available in audio/. Source and licensing links are included in the gallery.
