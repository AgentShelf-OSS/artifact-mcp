// SPDX-License-Identifier: Apache-2.0
// Organization-scoped viewer state persistence.
import db from "./db.js";
import { STATE_MAX_KEY_LENGTH, STATE_MAX_KEYS, STATE_MAX_VALUE_BYTES } from "./config.js";

// The negative lookahead requires the actual end of input; `$` also permits a final newline.
export const STATE_KEY_RE = new RegExp(`^[A-Za-z0-9._-]{1,${STATE_MAX_KEY_LENGTH}}(?![\\s\\S])`);

export function createStateStore({ db: database = db, now = () => new Date().toISOString().slice(0, 19).replace("T", " ") } = {}) {
  const listStmt = database.prepare("SELECT key, revision, updated_at FROM artifact_state WHERE artifact_id = ? ORDER BY key");
  const getStmt = database.prepare("SELECT key, value, revision, updated_at, updated_by FROM artifact_state WHERE artifact_id = ? AND key = ?");
  const countStmt = database.prepare("SELECT COUNT(*) AS count FROM artifact_state WHERE artifact_id = ?");
  const insertStmt = database.prepare("INSERT INTO artifact_state (artifact_id, key, value, revision, updated_at, updated_by) VALUES (?, ?, ?, 1, ?, ?)");
  const updateStmt = database.prepare("UPDATE artifact_state SET value = ?, revision = revision + 1, updated_at = ?, updated_by = ? WHERE artifact_id = ? AND key = ? AND (? IS NULL OR revision = ?)");
  const deleteStmt = database.prepare("DELETE FROM artifact_state WHERE artifact_id = ? AND key = ?");

  function list(artifactId) { return listStmt.all(artifactId); }
  function get(artifactId, key) {
    const row = getStmt.get(artifactId, key);
    if (!row) return null;
    return { ...row, value: JSON.parse(row.value) };
  }
  function put(artifactId, key, value, viewerEmail, ifRevision = null) {
    if (typeof key !== "string" || !STATE_KEY_RE.test(key)) return { ok: false, reason: "bad_key" };
    let serialized;
    try { serialized = JSON.stringify(value); } catch { return { ok: false, reason: "bad_value" }; }
    if (serialized === undefined || Buffer.byteLength(serialized, "utf8") > STATE_MAX_VALUE_BYTES) return { ok: false, reason: "too_large" };
    if (ifRevision !== null && (!Number.isSafeInteger(ifRevision) || ifRevision < 0)) return { ok: false, reason: "bad_revision" };
    const timestamp = now();
    return database.transaction(() => {
      const current = getStmt.get(artifactId, key);
      if (current && ifRevision !== null && current.revision !== ifRevision) {
        return { ok: false, reason: "conflict", value: JSON.parse(current.value), revision: current.revision };
      }
      if (!current) {
        if (ifRevision !== null && ifRevision !== 0) return { ok: false, reason: "conflict", value: null, revision: 0 };
        if (countStmt.get(artifactId).count >= STATE_MAX_KEYS) return { ok: false, reason: "too_many_keys" };
        insertStmt.run(artifactId, key, serialized, timestamp, viewerEmail);
        return { ok: true, key, revision: 1, updated_at: timestamp };
      }
      updateStmt.run(serialized, timestamp, viewerEmail, artifactId, key, ifRevision, ifRevision);
      return { ok: true, key, revision: current.revision + 1, updated_at: timestamp };
    }).immediate();
  }
  function remove(artifactId, key, afterDelete = () => {}) {
    database.transaction(() => {
      deleteStmt.run(artifactId, key);
      afterDelete();
    }).immediate();
  }
  return { list, get, put, remove };
}

const defaultStore = createStateStore({ db });
export const listState = defaultStore.list;
export const getState = defaultStore.get;
export const putState = defaultStore.put;
export const deleteState = defaultStore.remove;
