#!/usr/bin/env bash
# Consistent, verified backup of an artifact-mcp data directory.
#
# Why not `cp data/artifacts.db`: the database runs in WAL mode, so committed data lives in
# `artifacts.db-wal` until a checkpoint. Copying the .db alone yields a stale or torn backup.
#
# `VACUUM INTO` asks SQLite itself to write a consistent snapshot to a new file, taking a read
# transaction for the duration. No writer stop, no -wal/-shm to carry, and the result is already
# compacted. (https://sqlite.org/lang_vacuum.html#vacuuminto)
#
# The database snapshot is taken FIRST. Files are copied only after that cut, then the snapshot is
# checked against the copied bodies and retained history. A concurrent mutation can therefore make
# the verification fail, but can never silently publish a database row whose required body is
# absent from the completed backup.
#
# Usage:  backup.sh [DATA_DIR] [DEST_ROOT] [KEEP]
# Requires Python 3, plus sqlite3 when an artifacts.db file is present.
set -euo pipefail

DATA_DIR="${1:-${ARTIFACT_MCP_DATA_DIR:-./data}}"
DEST_ROOT="${2:-${ARTIFACT_MCP_BACKUP_DIR:-./backups}}"
KEEP="${3:-14}"

[[ "$KEEP" =~ ^0*[1-9][0-9]*$ ]] || { echo "backup: KEEP must be a positive integer" >&2; exit 1; }

[ -d "$DATA_DIR" ] || { echo "backup: DATA_DIR '$DATA_DIR' not found" >&2; exit 1; }

STAMP="$(date +%Y%m%d-%H%M%S-%N)"
mkdir -p "$DEST_ROOT"
command -v python3 >/dev/null 2>&1 || { echo "backup: Python 3 is required for coherence verification" >&2; exit 1; }
STAGE="$(mktemp -d "${DEST_ROOT}/.incomplete-${STAMP}-XXXXXX")"
FINAL="${DEST_ROOT}/backup-${STAMP}-${STAGE##*-}"

mkdir -p "$STAGE"
# A backup only becomes visible under its final name once every step has succeeded, so an
# interrupted run can never be mistaken for a good one.
trap 'rm -rf "$STAGE"' EXIT

# 1. Consistent database snapshot: this is the recovery point for the copied files.
DB="${DATA_DIR}/artifacts.db"
if [ -f "$DB" ]; then
  if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "backup: sqlite3 is required to snapshot and verify artifacts.db" >&2
    exit 1
  fi
  SQL_PATH="$(printf '%s' "${STAGE}/artifacts.db" | sed "s/'/''/g")"
  sqlite3 "$DB" "VACUUM INTO '$SQL_PATH'"
  [ -f "${STAGE}/artifacts.db" ] || { echo "backup: database snapshot was not created" >&2; exit 1; }
fi

# 2. Copy bodies after the database cut. cp -a preserves hidden staging/trash/history evidence.
[ -e "${DATA_DIR}/artifacts" ] && cp -a "${DATA_DIR}/artifacts" "${STAGE}/artifacts"
python3 "$(dirname "$0")/backup-coherence.py" "$STAGE" --capture-previews-from "${DATA_DIR}/previews"
if [ -e "${DATA_DIR}/previews" ]; then
  if ! cp -a "${DATA_DIR}/previews" "${STAGE}/previews"; then
    echo "backup: optional preview copy incomplete; checking files included" >&2
  fi
fi

# 3. Verify the snapshot before publishing it. An unverified backup is a guess.
VERIFY_ARGS=()
[ -f "$DB" ] && VERIFY_ARGS+=(--database-required)
python3 "$(dirname "$0")/backup-coherence.py" "$STAGE" --preview-manifest-required "${VERIFY_ARGS[@]}"
verify() {
  sqlite3 "$1" "PRAGMA quick_check;" | head -1
  sqlite3 "$1" "SELECT 'artifacts=' || count(*) FROM artifacts;"
}
if [ -f "${STAGE}/artifacts.db" ]; then
  OUT="$(verify "${STAGE}/artifacts.db")"
  echo "$OUT" | grep -qx "ok" || { echo "backup: integrity check FAILED:\n$OUT" >&2; exit 1; }
  COUNT="$(echo "$OUT" | grep '^artifacts=' || echo 'artifacts=?')"
else
  COUNT="artifacts=0 (no database)"
fi

# 4. Publish atomically, then prune.
mv "$STAGE" "$FINAL"
trap - EXIT
python3 - "$DEST_ROOT" <<'PY'
import os, sys
fd = os.open(sys.argv[1], os.O_RDONLY)
try:
    os.fsync(fd)
finally:
    os.close(fd)
PY
echo "backup: ${FINAL} ($(du -sh "$FINAL" | cut -f1), ${COUNT}, quick_check ok)"

ls -1d "${DEST_ROOT}"/backup-* 2>/dev/null | sort | head -n -"${KEEP}" | while read -r old; do
  rm -rf "$old" && echo "backup: pruned $(basename "$old")"
done
