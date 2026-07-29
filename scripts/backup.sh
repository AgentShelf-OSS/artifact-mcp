#!/usr/bin/env bash
# Consistent, verified backup of an artifact-mcp data directory.
#
# Why not `cp data/artifacts.db`: the database runs in WAL mode, so committed data lives in
# `artifacts.db-wal` until a checkpoint. Copying the .db alone yields a stale or torn backup — a
# A busy deployment can accumulate substantial committed data in that WAL.
#
# `VACUUM INTO` asks SQLite itself to write a consistent snapshot to a new file, taking a read
# transaction for the duration. No writer stop, no -wal/-shm to carry, and the result is already
# compacted. (https://sqlite.org/lang_vacuum.html#vacuuminto)
#
# Ordering matters: artifact BODIES are copied BEFORE the database snapshot. A body created after
# the copy is then absent from the DB snapshot too — an unreferenced file, which startup
# reconciliation reports and never deletes. The reverse order could produce the dangerous case: a
# database row referencing a body the backup does not contain.
#
# Usage:  backup.sh [DATA_DIR] [DEST_ROOT] [KEEP]
# Requires the sqlite3 CLI when an artifacts.db file is present.
set -euo pipefail

DATA_DIR="${1:-${ARTIFACT_MCP_DATA_DIR:-./data}}"
DEST_ROOT="${2:-${ARTIFACT_MCP_BACKUP_DIR:-./backups}}"
KEEP="${3:-14}"

[ -d "$DATA_DIR" ] || { echo "backup: DATA_DIR '$DATA_DIR' not found" >&2; exit 1; }

STAMP="$(date +%Y%m%d-%H%M%S)"
STAGE="${DEST_ROOT}/.incomplete-${STAMP}"
FINAL="${DEST_ROOT}/backup-${STAMP}"

mkdir -p "$STAGE"
# A backup only becomes visible under its final name once every step has succeeded, so an
# interrupted run can never be mistaken for a good one.
trap 'rm -rf "$STAGE"' EXIT

# 1. Bodies, previews and history first (see ordering note above).
for sub in artifacts previews; do
  [ -e "${DATA_DIR}/${sub}" ] && cp -a "${DATA_DIR}/${sub}" "${STAGE}/${sub}"
done

# 2. Consistent database snapshot.
DB="${DATA_DIR}/artifacts.db"
if [ -f "$DB" ]; then
  if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "backup: sqlite3 is required to snapshot and verify artifacts.db" >&2
    exit 1
  fi
  sqlite3 "$DB" "VACUUM INTO '${STAGE}/artifacts.db'"
fi

# 3. Verify the snapshot before publishing it. An unverified backup is a guess.
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
trap - EXIT
mv "$STAGE" "$FINAL"
echo "backup: ${FINAL} ($(du -sh "$FINAL" | cut -f1), ${COUNT}, quick_check ok)"

ls -1d "${DEST_ROOT}"/backup-* 2>/dev/null | sort | head -n -"${KEEP}" | while read -r old; do
  rm -rf "$old" && echo "backup: pruned $(basename "$old")"
done
