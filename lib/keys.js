// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
// Admin key management: generate, list, revoke upload API keys (per org).
// Secrets are shown once at creation and only stored hashed.
import crypto from "node:crypto";
import db from "./db.js";
import { sha256Hex } from "./auth.js";
import { verifiedOrgForEmail } from "./orgs.js";

const listStmt = db.prepare(
  "SELECT client_id, org, label, role, owner_email, created_at, revoked_at FROM api_keys ORDER BY (revoked_at IS NOT NULL), org, client_id"
);
const existsStmt = db.prepare("SELECT 1 FROM api_keys WHERE client_id = ?");
const keyStmt = db.prepare("SELECT client_id, org, owner_email FROM api_keys WHERE client_id = ?");
const insertStmt = db.prepare("INSERT INTO api_keys (client_id, org, label, role, owner_email, key_hash) VALUES (?, ?, ?, ?, ?, ?)");
const revokeStmt = db.prepare(
  "UPDATE api_keys SET revoked_at = datetime('now') WHERE client_id = ? AND revoked_at IS NULL"
);
const setOwnerStmt = db.prepare("UPDATE api_keys SET owner_email = ? WHERE client_id = ?");
const backfillCountStmt = db.prepare("SELECT COUNT(*) AS n FROM artifacts WHERE client_id = ? AND org = ? AND owner_email IS NULL");
const backfillOwnerStmt = db.prepare("UPDATE artifacts SET owner_email = ? WHERE client_id = ? AND org = ? AND owner_email IS NULL");

const NAME_RE = /^[a-z0-9][a-z0-9._-]{1,40}$/i;
const ORG_RE = /^[a-z0-9][a-z0-9._-]{0,40}$/i;
const KEY_ROLES = new Set(["reader", "author", "collaborator"]);
export const INVALID_KEY_ROLE_MESSAGE = "Role must be reader, author, or collaborator.";

export function listKeys() {
  return listStmt.all();
}

function normalizedVerifiedOwner(ownerEmail, org) {
  const owner = String(ownerEmail || "").trim().toLowerCase();
  if (!owner) return null;
  if (verifiedOrgForEmail(owner) !== org) {
    throw new Error("Owner must be a verified member of this organization.");
  }
  return owner;
}

export function createKey({ clientId, org, label, role = "author", ownerEmail }) {
  clientId = String(clientId || "").trim();
  org = String(org || "").trim();
  label = String(label || "").trim().slice(0, 60);
  role = String(role || "author").trim();
  if (!NAME_RE.test(clientId)) {
    throw new Error("Name must be 2–41 characters: letters, numbers, dot, dash, underscore.");
  }
  if (!ORG_RE.test(org)) {
    throw new Error("Org must be letters, numbers, dot, dash, or underscore.");
  }
  if (!KEY_ROLES.has(role)) {
    throw new Error(INVALID_KEY_ROLE_MESSAGE);
  }
  const ownerEmailNormalized = normalizedVerifiedOwner(ownerEmail, org);
  if (existsStmt.get(clientId)) {
    throw new Error(`A key named "${clientId}" already exists.`);
  }
  const secret = crypto.randomBytes(24).toString("hex");
  insertStmt.run(clientId, org, label, role, ownerEmailNormalized, sha256Hex(secret));
  return { clientId, org, label, role, ownerEmail: ownerEmailNormalized, secret };
}

export function revokeKey(clientId) {
  return revokeStmt.run(String(clientId || "")).changes > 0;
}

// Changing a key owner affects only future publications: each artifact holds an immutable
// publish-time snapshot.  An empty value deliberately converts a key back to a service key.
export function setKeyOwner(clientId, ownerEmail) {
  const key = keyStmt.get(String(clientId || ""));
  if (!key) return null;
  const owner = normalizedVerifiedOwner(ownerEmail, key.org);
  setOwnerStmt.run(owner, key.client_id);
  return { clientId: key.client_id, org: key.org, ownerEmail: owner };
}

const backfillOwner = db.transaction((clientId, ownerEmail, confirm) => {
  const key = keyStmt.get(String(clientId || ""));
  if (!key) return null;
  const owner = normalizedVerifiedOwner(ownerEmail, key.org);
  if (!owner) throw new Error("Owner is required for backfill.");
  const matched = Number(backfillCountStmt.get(key.client_id, key.org).n || 0);
  const updated = confirm === true ? backfillOwnerStmt.run(owner, key.client_id, key.org).changes : 0;
  return { clientId: key.client_id, org: key.org, ownerEmail: owner, matched, updated, confirmed: confirm === true };
});

// Preview is the default.  The only write is guarded by an exact `confirm === true` and by the
// SQL `owner_email IS NULL` predicate, so attributed rows can never be transferred here.
export function backfillKeyOwner(clientId, ownerEmail, { confirm = false } = {}) {
  return backfillOwner(clientId, ownerEmail, confirm);
}
