// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import crypto from "node:crypto";
import db from "./db.js";

export function sha256Hex(value) {
  return crypto.createHash("sha256").update(value).digest("hex");
}

const lookup = db.prepare("SELECT client_id, org, label, role, owner_email FROM api_keys WHERE key_hash = ? AND revoked_at IS NULL");

function bearer(req) {
  const auth = req.headers["authorization"] || "";
  const m = auth.match(/^\s*Bearer\s+(.+?)\s*$/i);
  if (m) return m[1].trim();
  const x = req.headers["x-api-key"];
  return (Array.isArray(x) ? x[0] : x || "").trim();
}

// Returns { ok, clientId } | { ok:false }. DB-backed, hashed, revocable.
export function checkKey(req) {
  const key = bearer(req);
  if (!key) return { ok: false };
  const row = lookup.get(sha256Hex(key));
  return row
    ? { ok: true, clientId: row.client_id, org: row.org, label: row.label || "", role: row.role || "author", ownerEmail: row.owner_email || null }
    : { ok: false };
}
