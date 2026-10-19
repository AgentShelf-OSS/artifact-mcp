#!/bin/sh
# Voice-Zero narrator voice pipeline on VM310. Usage: run.sh <step>
# Steps: fetch verify export jobs synth analyze f0 select
# Default bridge networking only for fetch; --network none everywhere else.
# At most two 1-CPU containers at a time (shared host).
# The gated weights come from the donation trial's Hugging Face cache, offline;
# no token is needed while that cache exists.
set -eu
T=/opt/docker/artifact-pocket-tts/trials
V=$T/voice-zero-20261019
D=$T/donations-20261018
POCKET=homelab/artifact-pocket:3.1.0-voicedir-20261018
ANALYSIS=homelab/pocket-voice-analysis:20261018
F0IMG=homelab/pocket-voice-analysis:20261018-f0
mkdir -p "$V/out" "$V/src"
chown 65532:65532 "$V" "$V/out"
case "$1" in
fetch)
  # Host python, standard library only; writes src/*.flac and sources.json.
  python3 -I "$V/scripts/vz_fetch.py" "$V" ;;
verify)
  # Must print the shipped andrea.safetensors hash prefix 1191c25b for "exact".
  docker run --rm --network none --cpus 1 -e HF_HOME=/hf -e HF_HUB_OFFLINE=1 \
    -v "$D:/work:ro" -v "$D/out:/out:ro" -v "$D/hf:/hf:ro" -v "$V/scripts:/scripts:ro" \
    "$POCKET" python /scripts/verify_export_procedure.py 1 ;;
export)
  docker run --rm --network none --cpus 1 -e HF_HOME=/hf -e HF_HUB_OFFLINE=1 \
    -v "$V:/work:ro" -v "$V/out:/out" -v "$D/hf:/hf:ro" -v "$V/scripts:/scripts:ro" \
    "$POCKET" python /scripts/export_voicezero.py ;;
jobs)
  python3 -I -c 'import json,sys
m=json.load(open(sys.argv[1]))
json.dump([{"voice":r["voice"],"kind":"candidate","prompt":"/out/states/%s.safetensors"%r["voice"]} for r in m["results"]],open(sys.argv[2],"w"))' \
    "$V/out/export-manifest.json" "$V/synth-jobs.json" ;;
synth)
  for s in 0 1; do
    # Production (ungated) model from the worker's own cache, as served to users.
    docker run -d --rm --name "vz-synth-$s" --network none --cpus 1 -e HF_HOME=/models/huggingface -e HF_HUB_OFFLINE=1 \
      -v "$V:/work:ro" -v "$V/out:/out" -v "$T/../models:/models:ro" -v "$V/scripts:/scripts:ro" \
      "$POCKET" python /scripts/synth.py "$s" 2
  done ;;
analyze)
  docker run -d --rm --name vz-analyze --network none --cpus 2 -e HF_HUB_OFFLINE=1 -e TRANSFORMERS_OFFLINE=1 \
    -v "$V:/work:ro" -v "$V/out:/out" -v "$D/hf-analysis:/hf:ro" -v "$V/scripts:/scripts:ro" \
    --entrypoint python "$ANALYSIS" /scripts/analyze.py ;;
f0)
  docker run --rm --network none --cpus 1 -v "$V:/work:ro" -v "$V/out:/out" -v "$V/scripts:/scripts:ro" \
    --entrypoint python "$F0IMG" /scripts/f0.py ;;
select)
  # catalog-live.json: read-only copy of the live CT220 catalog (92 voices). Baselines: the
  # donation and VCTK runs (same model, seed, and sentences) cover every exposed voice.
  docker run --rm --network none --cpus 1 -v "$V:/work:ro" -v "$V/out:/work/out" \
    -v "$D/out:/b1:ro" -v "$T/vctk-20261019/out:/b2:ro" -v "$V/scripts:/scripts:ro" \
    --entrypoint python "$ANALYSIS" /scripts/select_voicezero.py /work /work/catalog-live.json \
    /work/out/voice-metrics.json --baseline /b1 --baseline /b2 \
    --reserved-labels /work/scripts/vctk-voice-metrics.json --reserved-labels /work/scripts/label-overrides.json ;;
*) echo "unknown step $1" >&2; exit 2 ;;
esac
