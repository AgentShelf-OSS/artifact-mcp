# Voice-directory worker parity (2026-10-18)

`homelab/artifact-pocket:3.1.0-voicedir-20261018` (image ID `5a12b354…`) is
`Dockerfile.voice-dir` on top of `homelab/artifact-pocket:3.1.0-donated-20260914`
(`0f9b4454…`). Only `/app/server.py` and `/app/voices` change.

Pocket generation is stochastic. Two fresh starts of the old image gave
different WAV bytes for the same text, so HTTP byte comparison cannot prove
parity. `seeded.py` imports each image's `server` module, seeds torch with
1234 before each generation, and hashes the raw samples for 12 voices (3 presets,
5 Voice-Zero, 4 Kyutai donations), twice per voice.

| Run | Voice list + labels | Audio |
|---|---|---|
| old image | 35 voices | reference; each voice repeats exactly |
| new image, host dir at `/voices` | identical | identical for 12 of 12 |
| new image, bundled `/app/voices` | identical | identical for 12 of 12 |

The deployed workers were also checked through HTTP: WAV, PCM stream, timed
stream, and a cache hit for a legacy-key voice.

Rollback: set `image: homelab/artifact-pocket:3.1.0-donated-20260914` and remove
the `POCKET_TTS_VOICE_DIR` line and the `/voices` volume in both compose files
(backups: `compose.yml.pre-voice-dir-20261018-170928` in
`/opt/docker/artifact-pocket-tts` and `/opt/docker/artifact-pocket-multi`), then
`docker compose up -d` in each directory and run `nginx -s reload` in the gateway.
