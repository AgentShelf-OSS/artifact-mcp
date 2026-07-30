// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// PBI-058 security audit ledger. This module intentionally has no dependency on request bodies
// or notification delivery: callers provide a server-derived AuditContext and a small, allow-list
// shaped event. The event hash is HMAC-SHA-256 over a versioned length-prefixed byte record.
import crypto from "node:crypto";

export const AUDIT_RETENTION_DAYS = 180;
export const AUDIT_DEFAULT_LIMIT = 100;
export const AUDIT_MAX_LIMIT = 500;
export const AUDIT_EXPORT_MAX_ROWS = 10_000;
export const AUDIT_EXPORT_MAX_BYTES = 5 * 1024 * 1024;
export const AUDIT_CAPABILITIES = Object.freeze({ READ: "audit:read", EXPORT: "audit:export", GLOBAL: "audit:global" });

const ACTOR_TYPES = new Set(["api_key", "viewer", "system"]);
const RESULTS = new Set(["success", "denied", "failure", "recovered"]);
const SOURCES = new Set(["mcp", "browser", "maintenance", "reconciliation"]);
const FORBIDDEN_FIELD = /(secret|token|password|authorization|cookie|jwt|webhook|url|html|body|source|content)/i;
const FORBIDDEN_VALUE = /(bearer\s+|eyJ[a-zA-Z0-9_-]{8,}\.|discord(?:app)?\.com\/api\/webhooks\/|https?:\/\/)/i;
const SAFE_IDENTIFIER = /^[a-z][a-z0-9._-]{0,99}$/;
const PERSISTED_WEBHOOK_ID = /^[0-9abcdefghijkmnpqrstuvwxyz]{12}$/;

/** Exact `lib/webhooks.js` persisted row-id shape; URLs and bearer tokens never satisfy it. */
export function isValidAuditWebhookId(value) {
  const id = String(value ?? "");
  return id.length === 12 && PERSISTED_WEBHOOK_ID.test(id);
}

function text(value, max = 160) {
  return String(value ?? "").trim().slice(0, max);
}

function hmac(key, value) {
  return crypto.createHmac("sha256", key).update(value).digest("hex");
}

function constantEqual(left, right) {
  const a = Buffer.from(String(left));
  const b = Buffer.from(String(right));
  return a.length === b.length && crypto.timingSafeEqual(a, b);
}

const CANONICAL_VERSION = 1;
const HASH_DOMAIN = Buffer.from("artifact-mcp/security-audit/v1\0", "utf8");
const CANONICAL_FIELDS = ["event_id", "key_id", "tenant", "actor_type", "actor_id", "actor_role", "operation", "target_type", "target_id", "result", "classification", "source", "request_id", "revision", "occurred_at"];
const RECEIPT_FIELDS = ["correlation_id", "durability_intent_id", "state", "event_id", "tenant", "actor_type", "actor_id", "actor_role", "source", "request_id", "operation", "target_type", "target_id", "result", "classification", "revision", "key_id", "canonical_version"];

function u32(value) { const out = Buffer.alloc(4); out.writeUInt32BE(value); return out; }
function u64(value) { const out = Buffer.alloc(8); out.writeBigUInt64BE(BigInt(value)); return out; }
function part(value) { const bytes = Buffer.from(String(value ?? ""), "utf8"); return Buffer.concat([u32(bytes.length), bytes]); }
/** Frozen wire format: version byte followed by length-prefixed UTF-8 fields in CANONICAL_FIELDS order. */
export function canonicalAuditBytes(row) { return Buffer.concat([Buffer.from([CANONICAL_VERSION]), ...CANONICAL_FIELDS.map((field) => part(row[field]))]); }
/** Frozen receipt format shared with Rust; every immutable reservation input is authenticated. */
export function canonicalReceiptBytes(row) {
  return Buffer.concat([Buffer.from([row.canonical_version]), ...RECEIPT_FIELDS.map((field) => part(row[field]))]);
}
function hashEvent(key, sequence, prevHash, canonical) { return hmac(key, Buffer.concat([HASH_DOMAIN, u64(sequence), part(prevHash), u32(canonical.length), canonical])); }
function hashHead(key, keyId, sequence, headHash, pendingRoot) { return hmac(key, Buffer.concat([HASH_DOMAIN, Buffer.from("head\0"), part(keyId), Buffer.from([CANONICAL_VERSION]), u64(sequence), part(headHash), part(pendingRoot)])); }
function hashReceipt(key, row) { return hmac(key, Buffer.concat([HASH_DOMAIN, Buffer.from("receipt\0"), canonicalReceiptBytes(row)])); }
export function pendingReceiptsRoot(receiptMacs) {
  const sorted = [...receiptMacs].map(String).sort();
  return crypto.createHash("sha256").update(Buffer.concat([
    HASH_DOMAIN, Buffer.from("pending-receipts\0"), u64(sorted.length), ...sorted.map(part)
  ])).digest("hex");
}

function keyFrom(value) {
  const raw = String(value || "").trim();
  if (!raw) return null;
  if (!/^[A-Za-z0-9+/]+={0,2}$/.test(raw) || raw.length % 4 !== 0) return null;
  const decoded = Buffer.from(raw, "base64");
  return decoded.length === 32 && decoded.toString("base64") === raw ? decoded : null;
}

/** Fail closed before accepting traffic; do not log the value. */
export function assertAuditReady(value = process.env.AUDIT_LEDGER_HMAC_KEY) {
  if (!keyFrom(value)) {
    throw new Error("AUDIT_LEDGER_HMAC_KEY is required and must be canonical base64 for exactly 32 bytes; refusing to start without a tamper-evident audit ledger");
  }
}

/** Construct only from an already-verified authentication result, never request input. */
export function auditContextFromAuth(auth, { source, requestId = "" } = {}) {
  if (!auth?.ok || !auth.clientId || !auth.org) throw new Error("AuditContext requires verified publisher authentication");
  return Object.freeze({
    tenant: text(auth.org, 80), actorType: "api_key", actorId: text(auth.clientId, 120),
    actorRole: text(auth.role || "author", 40), source: source === "mcp" ? "mcp" : "maintenance",
    requestId: text(requestId, 120)
  });
}

/** Construct only from the JWT/header resolver's verified viewer projection. */
export function auditContextFromViewer(viewer, { source = "browser", requestId = "" } = {}) {
  if (!viewer?.email || !viewer?.org) throw new Error("AuditContext requires verified viewer authentication");
  return Object.freeze({
    tenant: text(viewer.org, 80), actorType: "viewer", actorId: text(viewer.email, 160),
    actorRole: viewer.isAdmin ? "admin" : "member", source: source === "browser" ? "browser" : "maintenance",
    requestId: text(requestId, 120)
  });
}

export function systemAuditContext({ source = "maintenance", requestId = "" } = {}) {
  if (!SOURCES.has(source) || source === "mcp" || source === "browser") throw new Error("Invalid system audit source");
  return Object.freeze({ tenant: "system", actorType: "system", actorId: "artifact-mcp", actorRole: "", source, requestId: text(requestId, 120) });
}

function normalizeEvent(context, event) {
  if (!context || !ACTOR_TYPES.has(context.actorType) || !SOURCES.has(context.source)) throw new Error("Invalid server-derived AuditContext");
  if (!event || typeof event !== "object" || Array.isArray(event)) throw new Error("Audit event must be an object");
  for (const [name, value] of Object.entries(event)) {
    if (FORBIDDEN_FIELD.test(name) || (typeof value === "string" && FORBIDDEN_VALUE.test(value))) {
      throw new Error(`Audit event contains a prohibited sensitive field: ${name}`);
    }
  }
  const result = text(event.result, 20);
  if (!RESULTS.has(result)) throw new Error("Invalid audit result");
  const operation = text(event.operation, 100);
  const targetType = text(event.targetType, 60);
  const classification = text(event.classification, 80);
  const targetId = text(event.targetId, 160);
  if (!SAFE_IDENTIFIER.test(operation) || !SAFE_IDENTIFIER.test(targetType) || (classification && !SAFE_IDENTIFIER.test(classification))) {
    throw new Error("Audit operation, targetType, and classification must be server-controlled identifiers");
  }
  // Public share credentials and webhook endpoints are not identifiers suitable for the ledger.
  // An authorized webhook test may retain only the persisted 12-character row id resolved from
  // its path. The exact generator alphabet/length prevents an arbitrary token-shaped value from
  // being smuggled through the otherwise broader SAFE_IDENTIFIER grammar.
  const webhookTarget = /webhook/i.test(targetType);
  if ((targetId && webhookTarget && !isValidAuditWebhookId(targetId))
      || (targetId && !webhookTarget && !SAFE_IDENTIFIER.test(targetId))
      || (/share/i.test(targetType) && targetId)) {
    throw new Error("Audit events must not retain share tokens or unvalidated webhook identifiers");
  }
  const revision = event.revision === undefined || event.revision === null ? null : Number(event.revision);
  if (revision !== null && (!Number.isSafeInteger(revision) || revision < 0)) throw new Error("Invalid audit revision");
  return {
    tenant: text(context.tenant, 80), actor_type: context.actorType, actor_id: text(context.actorId, 160), actor_role: text(context.actorRole, 40),
    operation, target_type: targetType, target_id: targetId, result,
    classification, source: context.source, request_id: text(context.requestId, 120), revision
  };
}

function receiptIdentifier(value) {
  const identifier = String(value ?? "");
  if (!identifier || identifier.length > 240 || identifier.includes("\0")) throw new Error("Invalid audit receipt identifier");
  return identifier;
}

export function createAuditLedger({ db, hmacKey = process.env.AUDIT_LEDGER_HMAC_KEY, keyId = "v1", now = () => new Date().toISOString(), id = () => crypto.randomUUID(), exportMaxBytes = AUDIT_EXPORT_MAX_BYTES } = {}) {
  const key = keyFrom(hmacKey);
  if (!db || !key) throw new Error("Audit ledger requires SQLite and a 32-byte HMAC key");
  const head = db.prepare("SELECT sequence, key_id, head_hash, head_mac, canonical_version, pending_receipts_root FROM security_audit_chain_head WHERE singleton = 1");
  const insert = db.prepare(`INSERT INTO security_audit_events
    (sequence, event_id, key_id, tenant, actor_type, actor_id, actor_role, operation, target_type, target_id, result, classification, source, request_id, revision, occurred_at, canonical_version, canonical, prev_hash, event_hash)
    VALUES (@sequence, @event_id, @key_id, @tenant, @actor_type, @actor_id, @actor_role, @operation, @target_type, @target_id, @result, @classification, @source, @request_id, @revision, @occurred_at, @canonical_version, @canonical, @prev_hash, @event_hash)`);
  const updateHead = db.prepare("UPDATE security_audit_chain_head SET sequence = ?, key_id = ?, head_hash = ?, pending_receipts_root = ?, head_mac = ?, updated_at = datetime('now') WHERE singleton = 1 AND sequence = ? AND head_hash = ? AND head_mac = ? AND pending_receipts_root = ?");
  const pendingReceiptMacs = db.prepare("SELECT receipt_mac FROM security_audit_receipts WHERE state = 'pending' ORDER BY receipt_mac ASC");
  const pendingReceipts = db.prepare("SELECT event_id, state, correlation_id, durability_intent_id, tenant, actor_type, actor_id, actor_role, source, request_id, operation, target_type, target_id, result, classification, revision, key_id, canonical_version, receipt_mac FROM security_audit_receipts WHERE state = 'pending' ORDER BY correlation_id ASC");
  const receipt = db.prepare("SELECT event_id, state, correlation_id, durability_intent_id, tenant, actor_type, actor_id, actor_role, source, request_id, operation, target_type, target_id, result, classification, revision, key_id, canonical_version, receipt_mac FROM security_audit_receipts WHERE correlation_id = ?");
  const reserveReceipt = db.prepare(`INSERT INTO security_audit_receipts
    (correlation_id, durability_intent_id, state, operation, target_type, target_id, result, tenant, actor_type, actor_id, actor_role, source, request_id, revision, classification, key_id, canonical_version, receipt_mac)
    VALUES (@correlation_id, @durability_intent_id, 'pending', @operation, @target_type, @target_id, @result, @tenant, @actor_type, @actor_id, @actor_role, @source, @request_id, @revision, @classification, @key_id, @canonical_version, @receipt_mac)`);
  const finalizeReceiptStmt = db.prepare("UPDATE security_audit_receipts SET event_id = ?, state = 'finalized', receipt_mac = ?, finalized_at = datetime('now') WHERE correlation_id = ? AND state = 'pending' AND receipt_mac = ?");
  const receiptEvent = db.prepare("SELECT event_id, key_id, tenant, actor_type, actor_id, actor_role, operation, target_type, target_id, result, classification, source, request_id, revision, canonical_version, sequence, event_hash FROM security_audit_events WHERE event_id = ?");
  function currentPendingRoot() {
    return pendingReceiptsRoot(pendingReceiptMacs.all().map((row) => row.receipt_mac));
  }
  function validHead(current) {
    return current
      && current.canonical_version === CANONICAL_VERSION
      && current.key_id === keyId
      && constantEqual(current.head_mac, hashHead(key, keyId, current.sequence, current.head_hash, current.pending_receipts_root));
  }
  function verifyPendingCommitment() {
    const current = head.get();
    if (!validHead(current) || current.pending_receipts_root !== currentPendingRoot()) {
      throw new Error("Audit pending receipt commitment verification failed");
    }
    for (const pending of pendingReceipts.all()) verifyReceiptSnapshot(pending);
    return current;
  }
  function refreshPendingCommitment() {
    const current = head.get();
    if (!validHead(current)) throw new Error("Audit chain head is missing or incompatible");
    const root = currentPendingRoot();
    const signature = hashHead(key, keyId, current.sequence, current.head_hash, root);
    if (updateHead.run(current.sequence, keyId, current.head_hash, root, signature,
      current.sequence, current.head_hash, current.head_mac, current.pending_receipts_root).changes !== 1) {
      throw new Error("Audit pending receipt commitment changed concurrently");
    }
  }
  db.transaction(() => {
    const current = head.get();
    if (!current || current.canonical_version !== CANONICAL_VERSION || current.key_id !== keyId) throw new Error("Audit chain head is missing or incompatible");
    if (current.sequence === 0 && current.head_hash === "" && current.head_mac === "" && current.pending_receipts_root === "") {
      const root = currentPendingRoot();
      updateHead.run(0, keyId, "", root, hashHead(key, keyId, 0, "", root), 0, "", "", "");
    }
    verifyPendingCommitment();
  })();

  function appendInTransaction(context, event) {
    verifyPendingCommitment();
    const row = normalizeEvent(context, event);
    if (event.occurredAt !== undefined) throw new Error("Audit timestamps are server-generated");
    const occurred_at = text(now(), 40);
    const current = head.get();
    if (!current || current.canonical_version !== CANONICAL_VERSION) throw new Error("Audit chain head is missing or incompatible");
    const sequence = current.sequence + 1;
    const event_id = id();
    const canonical = canonicalAuditBytes({ ...row, event_id, key_id: keyId, revision: row.revision ?? "", occurred_at });
    const prev_hash = current.head_hash;
    const event_hash = hashEvent(key, sequence, prev_hash, canonical);
    insert.run({ ...row, sequence, event_id, key_id: keyId, occurred_at, canonical_version: CANONICAL_VERSION, canonical, prev_hash, event_hash });
    if (updateHead.run(sequence, keyId, event_hash, current.pending_receipts_root,
      hashHead(key, keyId, sequence, event_hash, current.pending_receipts_root),
      current.sequence, current.head_hash, current.head_mac, current.pending_receipts_root).changes !== 1) throw new Error("Audit chain head changed concurrently");
    return { eventId: event_id, eventHash: event_hash, sequence };
  }
  const append = db.transaction((context, event) => appendInTransaction(context, event));

  function cursorFor(tenant, lastSequence) {
    const payload = Buffer.from(JSON.stringify({ v: 1, tenant, lastSequence }), "utf8").toString("base64url");
    return `${payload}.${hmac(key, Buffer.concat([HASH_DOMAIN, Buffer.from("cursor\0"), Buffer.from(payload, "utf8")]))}`;
  }
  function cursorFrom(cursor, tenant) {
    if (!cursor) return 0;
    const [payload, signature, extra] = String(cursor).split(".");
    const expected = hmac(key, Buffer.concat([HASH_DOMAIN, Buffer.from("cursor\0"), Buffer.from(payload || "", "utf8")]));
    const supplied = Buffer.from(signature || "");
    const expectedBytes = Buffer.from(expected);
    if (extra || !signature || supplied.length !== expectedBytes.length || !crypto.timingSafeEqual(supplied, expectedBytes)) throw new Error("Invalid audit cursor");
    let value; try { value = JSON.parse(Buffer.from(payload, "base64url").toString("utf8")); } catch { throw new Error("Invalid audit cursor"); }
    if (value?.v !== 1 || value.tenant !== tenant || !Number.isSafeInteger(value.lastSequence) || value.lastSequence < 0) throw new Error("Audit cursor does not match tenant");
    return value.lastSequence;
  }
  function query(context, { tenant = context?.tenant, cursor, limit = AUDIT_DEFAULT_LIMIT } = {}) {
    requireAuditCapability(context, AUDIT_CAPABILITIES.READ, tenant);
    const bounded = Math.min(AUDIT_MAX_LIMIT, Math.max(1, Number(limit) || AUDIT_DEFAULT_LIMIT));
    const after = cursorFrom(cursor, tenant);
    const rows = db.prepare(`SELECT sequence, event_id, tenant, operation, target_type, target_id, result, classification, occurred_at, event_hash
      FROM security_audit_events WHERE tenant = ? AND sequence > ? ORDER BY sequence ASC LIMIT ?`).all(tenant, after, bounded + 1);
    const events = rows.slice(0, bounded);
    return { events, next: rows.length > bounded ? cursorFor(tenant, events.at(-1).sequence) : null };
  }
  function exportEvents(context, options = {}) {
    const tenant = options.tenant || context?.tenant;
    requireAuditCapability(context, AUDIT_CAPABILITIES.EXPORT, tenant);
    const wanted = Math.min(AUDIT_EXPORT_MAX_ROWS, Math.max(1, Number(options.limit) || AUDIT_EXPORT_MAX_ROWS));
    requireAuditCapability(context, AUDIT_CAPABILITIES.READ, tenant);
    const after = cursorFrom(options.cursor, tenant);
    // Export is deliberately not built on the interactive 500-row query page: it has its own
    // fixed 10k/5MiB ceiling and still retains the tenant predicate + sequence ordering.
    const events = db.prepare(`SELECT sequence, event_id, tenant, operation, target_type, target_id, result, classification, occurred_at, event_hash
      FROM security_audit_events WHERE tenant = ? AND sequence > ? ORDER BY sequence ASC LIMIT ?`).all(tenant, after, wanted + 1);
    const rows = [];
    let lastSequence = null;
    let bytes = 0;
    for (const row of events.slice(0, wanted)) {
      const line = `${JSON.stringify(row)}\n`;
      const lineBytes = Buffer.byteLength(line);
      if (bytes + lineBytes > exportMaxBytes) break;
      rows.push(line); bytes += lineBytes; lastSequence = row.sequence;
    }
    const hasMore = rows.length < events.length;
    // A single pathological row cannot be emitted safely. Do not return the unchanged
    // cursor, which would make clients retry forever; operators must remediate the row.
    const next = rows.length ? (hasMore ? cursorFor(tenant, lastSequence) : null) : null;
    return { ndjson: rows.join(""), rows: rows.length, bytes, truncated: hasMore, next,
      reason: !rows.length && hasMore ? "first_row_exceeds_export_cap" : null };
  }
  function verify() {
    const checkpoints = db.prepare("SELECT first_sequence, last_sequence, key_id, canonical_version, bridge_hash, prev_checkpoint_hash, checkpoint_hash FROM security_audit_checkpoints ORDER BY checkpoint_id ASC").all();
    let previousCheckpoint = "";
    let expectedCheckpointFirst = 1;
    for (const checkpoint of checkpoints) {
      const expectedCheckpoint = hmac(key, Buffer.concat([HASH_DOMAIN, Buffer.from("checkpoint\0"), u64(checkpoint.first_sequence), u64(checkpoint.last_sequence), part(checkpoint.key_id), Buffer.from([checkpoint.canonical_version]), part(checkpoint.bridge_hash), part(previousCheckpoint)]));
      if (checkpoint.first_sequence !== expectedCheckpointFirst || checkpoint.last_sequence < checkpoint.first_sequence || checkpoint.key_id !== keyId || checkpoint.canonical_version !== CANONICAL_VERSION || checkpoint.prev_checkpoint_hash !== previousCheckpoint || !constantEqual(checkpoint.checkpoint_hash, expectedCheckpoint)) return { ok: false, checkpoint: checkpoint.last_sequence };
      previousCheckpoint = checkpoint.checkpoint_hash;
      expectedCheckpointFirst = checkpoint.last_sequence + 1;
    }
    const bridge = checkpoints.at(-1)?.bridge_hash || "";
    let previous = bridge;
    const rows = db.prepare("SELECT sequence, event_id, key_id, tenant, actor_type, actor_id, actor_role, operation, target_type, target_id, result, classification, source, request_id, revision, occurred_at, canonical_version, canonical, prev_hash, event_hash FROM security_audit_events ORDER BY sequence ASC").all();
    if (rows.length && rows[0].sequence !== expectedCheckpointFirst) return { ok: false, sequence: rows[0].sequence };
    let expectedSequence = expectedCheckpointFirst;
    for (const row of rows) {
      const recomputed = canonicalAuditBytes({ ...row, revision: row.revision ?? "" });
      if (row.sequence !== expectedSequence || row.canonical_version !== CANONICAL_VERSION || row.key_id !== keyId || !Buffer.from(row.canonical).equals(recomputed) || row.prev_hash !== previous || row.event_hash !== hashEvent(key, row.sequence, previous, row.canonical)) return { ok: false, sequence: row.sequence };
      previous = row.event_hash;
      expectedSequence += 1;
    }
    const current = head.get();
    const pendingRoot = currentPendingRoot();
    return current?.sequence === (rows.at(-1)?.sequence ?? checkpoints.at(-1)?.last_sequence ?? 0)
      && current?.head_hash === previous
      && current?.key_id === keyId
      && current?.pending_receipts_root === pendingRoot
      && constantEqual(current?.head_mac, hashHead(key, keyId, current?.sequence ?? 0, previous, pendingRoot))
      ? { ok: true, events: rows.length, head: previous } : { ok: false, head: true };
  }
  const prune = db.transaction((cutoff = new Date(Date.now() - AUDIT_RETENTION_DAYS * 86400_000).toISOString(), batchSize = 1_000) => {
    // `occurred_at` is duplicated inside canonical bytes. Never let a modified timestamp decide
    // which irreversible prefix is deleted.
    if (!verify().ok) throw new Error("Audit ledger integrity verification failed before retention");
    const batch = Math.min(1_000, Math.max(1, Number(batchSize) || 1_000));
    const prefix = db.prepare("SELECT sequence, event_hash, occurred_at FROM security_audit_events ORDER BY sequence ASC LIMIT ?").all(batch);
    let count = 0;
    for (const row of prefix) { if (row.occurred_at >= cutoff) break; count += 1; }
    if (!count) return 0;
    const first = prefix[0].sequence;
    const boundary = prefix[count - 1];
    const previousCheckpoint = db.prepare("SELECT checkpoint_hash FROM security_audit_checkpoints ORDER BY checkpoint_id DESC LIMIT 1").get()?.checkpoint_hash || "";
    const checkpointHash = hmac(key, Buffer.concat([HASH_DOMAIN, Buffer.from("checkpoint\0"), u64(first), u64(boundary.sequence), part(keyId), Buffer.from([CANONICAL_VERSION]), part(boundary.event_hash), part(previousCheckpoint)]));
    db.prepare("INSERT INTO security_audit_checkpoints (first_sequence, last_sequence, key_id, canonical_version, bridge_hash, prev_checkpoint_hash, checkpoint_hash) VALUES (?, ?, ?, ?, ?, ?, ?)").run(first, boundary.sequence, keyId, CANONICAL_VERSION, boundary.event_hash, previousCheckpoint, checkpointHash);
    return db.prepare("DELETE FROM security_audit_events WHERE sequence >= ? AND sequence <= ?").run(first, boundary.sequence).changes;
  });
  function verifyReceiptSnapshot(existing) {
    const eventId = existing.event_id ?? "";
    const validState = (existing.state === "pending" && eventId === "")
      || (existing.state === "finalized" && eventId !== "");
    if (!validState
      || existing.key_id !== keyId
      || existing.canonical_version !== CANONICAL_VERSION
      || !constantEqual(existing.receipt_mac, hashReceipt(key, {
        ...existing, event_id: eventId, revision: existing.revision ?? ""
      }))) {
      throw new Error("Audit receipt integrity verification failed");
    }
  }
  function verifyFinalizedProjection(existing) {
    const persisted = receiptEvent.get(existing.event_id);
    const fields = ["event_id", "key_id", "tenant", "actor_type", "actor_id", "actor_role", "operation",
      "target_type", "target_id", "result", "classification", "source", "request_id", "revision",
      "canonical_version"];
    if (!persisted || fields.some((field) => (persisted[field] ?? null) !== (existing[field] ?? null))) {
      throw new Error("Finalized audit receipt event projection mismatch");
    }
    return persisted;
  }
  const finalizeReceipt = db.transaction((correlationId, context, event) => {
    verifyPendingCommitment();
    const existing = receipt.get(correlationId);
    if (!existing) throw new Error("Audit receipt was not reserved");
    verifyReceiptSnapshot(existing);
    const normalized = normalizeEvent(context, event);
    for (const field of ["tenant", "actor_type", "actor_id", "actor_role", "source", "request_id", "operation", "target_type", "target_id", "result", "classification", "revision"]) {
      if ((existing[field] ?? null) !== (normalized[field] ?? null)) throw new Error("Audit receipt context mismatch");
    }
    if (existing.state === "finalized") {
      const persisted = verifyFinalizedProjection(existing);
      return { eventId: persisted.event_id, eventHash: persisted.event_hash, sequence: persisted.sequence, duplicate: true };
    }
    const written = appendInTransaction(context, event);
    const finalizedMac = hashReceipt(key, {
      ...existing, state: "finalized", event_id: written.eventId, revision: existing.revision ?? ""
    });
    if (finalizeReceiptStmt.run(written.eventId, finalizedMac, correlationId, existing.receipt_mac).changes !== 1) throw new Error("Audit receipt finalization conflict");
    refreshPendingCommitment();
    return { ...written, duplicate: false };
  });
  function reserveReceiptInTransaction(correlationId, intentId, context, event) {
    verifyPendingCommitment();
    const normalized = normalizeEvent(context, event);
    const row = {
      ...normalized,
      correlation_id: receiptIdentifier(correlationId),
      durability_intent_id: receiptIdentifier(intentId),
      state: "pending",
      event_id: "",
      key_id: keyId,
      canonical_version: CANONICAL_VERSION,
    };
    row.receipt_mac = hashReceipt(key, { ...row, revision: row.revision ?? "" });
    const inserted = reserveReceipt.run(row).changes === 1;
    if (inserted) refreshPendingCommitment();
    return inserted;
  }
  const reserveReceiptTransaction = db.transaction(reserveReceiptInTransaction);
  return { append, appendInTransaction, reserveReceipt: reserveReceiptTransaction, reserveReceiptInTransaction, finalizeReceipt, query, export: exportEvents, verify, prune };
}

function capabilities(context) { return new Set(context?.capabilities || []); }
export function requireAuditCapability(context, required, tenant) {
  const caps = capabilities(context);
  if (!caps.has(required)) throw new Error("Audit capability is required");
  if (tenant !== context.tenant && !caps.has(AUDIT_CAPABILITIES.GLOBAL)) throw new Error("Global audit capability is required");
}

export function createAuditMetrics() {
  const counters = new Map();
  return {
    record(signal) { if (["auth_failure", "admin_action", "integrity_failure", "reconciliation_failure", "rate_limit", "dead_letter_growth"].includes(signal)) counters.set(signal, (counters.get(signal) || 0) + 1); },
    renderPrometheus() { return [...counters.entries()].sort().map(([signal, value]) => `artifact_mcp_security_audit_signals_total{signal="${signal}"} ${value}\n`).join(""); }
  };
}
