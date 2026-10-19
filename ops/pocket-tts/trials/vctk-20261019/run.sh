#!/bin/sh
# VCTK voice pipeline on VM310. Usage: run.sh <step>
# Steps: refs export jobs synth analyze f0
# Default bridge networking only, and --network none where nothing is downloaded.
# At most two 1-CPU containers at a time (shared host).
# The gated weights come from the donation trial's Hugging Face cache, offline;
# no token is needed while that cache exists.
set -eu
T=/opt/docker/artifact-pocket-tts/trials
V=$T/vctk-20261019
D=$T/donations-20261018
POCKET=homelab/artifact-pocket:3.1.0-voicedir-20261018
ANALYSIS=homelab/pocket-voice-analysis:20261018
F0IMG=homelab/pocket-voice-analysis:20261018-f0
mkdir -p "$V/out" "$V/refs"
chown 65532:65532 "$V" "$V/out" "$V/refs"
case "$1" in
refs)
  docker run --rm --network none --cpus 1 -v "$V/src:/src:ro" -v "$V:/w" -v "$V/scripts:/scripts:ro" \
    --entrypoint python "$ANALYSIS" /scripts/build_refs.py /src /w ;;
export)
  cp "$V/src/speaker-info.txt" "$V/speaker-info.txt"
  docker run --rm --network none --cpus 1 -e HF_HOME=/hf -e HF_HUB_OFFLINE=1 \
    -v "$V:/work:ro" -v "$V/out:/out" -v "$D/hf:/hf:ro" -v "$V/scripts:/scripts:ro" \
    "$POCKET" python /scripts/export_vctk.py ;;
jobs)
  python3 -I -c 'import json,sys
m=json.load(open(sys.argv[1]))
json.dump([{"voice":r["voice"],"kind":"candidate","prompt":"/out/states/%s.safetensors"%r["voice"]} for r in m["results"]],open(sys.argv[2],"w"))' \
    "$V/out/export-manifest.json" "$V/synth-jobs.json" ;;
synth)
  for s in 0 1; do
    # Production (ungated) model from the worker's own cache, as served to users.
    docker run -d --rm --name "vctk-synth-$s" --network none --cpus 1 -e HF_HOME=/models/huggingface -e HF_HUB_OFFLINE=1 \
      -v "$V:/work:ro" -v "$V/out:/out" -v "$T/../models:/models:ro" -v "$V/scripts:/scripts:ro" \
      "$POCKET" python /scripts/synth.py "$s" 2
  done ;;
analyze)
  docker run -d --rm --name vctk-analyze --network none --cpus 2 -e HF_HUB_OFFLINE=1 -e TRANSFORMERS_OFFLINE=1 \
    -v "$V:/work:ro" -v "$V/out:/out" -v "$D/hf-analysis:/hf:ro" -v "$V/scripts:/scripts:ro" \
    --entrypoint python "$ANALYSIS" /scripts/analyze.py ;;
f0)
  docker run --rm --network none --cpus 1 -v "$V:/work:ro" -v "$V/out:/out" -v "$V/scripts:/scripts:ro" \
    --entrypoint python "$F0IMG" /scripts/f0.py ;;
*) echo "unknown step $1" >&2; exit 2 ;;
esac
