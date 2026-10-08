// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
// ADR-0012 scheduled reminders and Web Push subscriptions. The Node reference stores and validates
// reminders, opt-ins, and subscriptions. It never sends a push: only the Rust runtime runs the
// sweeper and the push sender (ADR-0012 item 10).
import { ECDH, createHash, randomUUID } from "node:crypto";
import { isIP } from "node:net";
import { WEB_PUSH } from "./config.js";
import { decrypt, encrypt } from "./crypto.js";

export const REMINDER_KEY_RE = /^[A-Za-z0-9._-]{1,64}(?![\s\S])/;
export const REMINDER_MIN_LEAD_MS = 60 * 1000;
export const REMINDER_MAX_LEAD_MS = 30 * 24 * 60 * 60 * 1000;
export const REMINDER_TITLE_MAX_CHARS = 80;
export const REMINDER_BODY_MAX_CHARS = 240;
export const REMINDER_ARMED_LIMIT = 16;
export const PUSH_SUBSCRIPTIONS_PER_VIEWER = 10;
export const PUSH_ENDPOINT_MAX_BYTES = 1024;
export const PUSH_LABEL_MAX_CHARS = 60;
export const PUSH_DISABLED_MESSAGE = "Web Push reminders are not configured on this server.";

// Unicode general category Cc (what Rust's char::is_control reports), except "\n".
const FORBIDDEN_CONTROL_RE = /[\u0000-\u0009\u000b-\u001f\u007f-\u009f]/u;
const BASE64URL_RE = /^[A-Za-z0-9_-]+={0,2}$/;

function sqlNow() {
  return new Date().toISOString().slice(0, 19).replace("T", " ");
}

function normalizeEmail(value) {
  return typeof value === "string" ? value.trim().toLowerCase() : "";
}

function base64urlBytes(value) {
  if (typeof value !== "string" || !BASE64URL_RE.test(value)) return null;
  const unpadded = value.replace(/=+$/, "");
  const bytes = Buffer.from(unpadded, "base64url");
  // Reject non-canonical trailing bits so both runtimes agree on what a value decodes to.
  return bytes.toString("base64url") === unpadded ? bytes : null;
}

/** Host allowlist match: exact names, or `*.suffix` entries that match subdomains only. */
export function hostAllowed(hostname, allowlist) {
  const host = String(hostname || "").toLowerCase();
  // An IP-literal endpoint never matches, whatever the allowlist holds.
  if (!host || host.startsWith("[") || isIP(host)) return false;
  return allowlist.some((entry) => entry.startsWith("*.")
    ? host.endsWith(entry.slice(1)) && host.length > entry.length - 1
    : host === entry);
}

export function validPushEndpoint(endpoint, allowlist) {
  if (typeof endpoint !== "string" || !endpoint || Buffer.byteLength(endpoint, "utf8") > PUSH_ENDPOINT_MAX_BYTES) return false;
  let url;
  try { url = new URL(endpoint); } catch { return false; }
  if (url.protocol !== "https:" || url.username || url.password) return false;
  // WHATWG URL parsing drops the default port, so an explicit ":443" also yields "".
  if (url.port !== "") return false;
  return hostAllowed(url.hostname, allowlist);
}

export function validPushKeys(keys) {
  if (!keys || typeof keys !== "object" || Array.isArray(keys)) return false;
  const p256dh = base64urlBytes(keys.p256dh);
  const auth = base64urlBytes(keys.auth);
  if (!p256dh || p256dh.length !== 65 || p256dh[0] !== 0x04) return false;
  if (!auth || auth.length !== 16) return false;
  try { ECDH.convertKey(p256dh, "prime256v1"); } catch { return false; }
  return true;
}

/** Validate title/body. Returns the normalized text or null. */
export function reminderText(title, body) {
  if (typeof title !== "string") return null;
  if (body !== undefined && typeof body !== "string") return null;
  const normalizedTitle = title.trim();
  const normalizedBody = body ?? "";
  const titleChars = [...normalizedTitle].length;
  if (titleChars < 1 || titleChars > REMINDER_TITLE_MAX_CHARS) return null;
  if ([...normalizedBody].length > REMINDER_BODY_MAX_CHARS) return null;
  if (FORBIDDEN_CONTROL_RE.test(normalizedTitle) || FORBIDDEN_CONTROL_RE.test(normalizedBody)) return null;
  return { title: normalizedTitle, body: normalizedBody };
}

/** Resolve exactly one of fire_at (epoch ms) or delay_seconds to an epoch-ms fire time, or null. */
export function reminderFireAt({ fireAt, delaySeconds }, nowMs) {
  // A field that is present counts, even when it is null; null is then rejected as not an integer.
  const hasFireAt = fireAt !== undefined;
  const hasDelay = delaySeconds !== undefined;
  if (hasFireAt === hasDelay) return null;
  let resolved;
  if (hasFireAt) {
    if (!Number.isSafeInteger(fireAt)) return null;
    resolved = fireAt;
  } else {
    if (!Number.isSafeInteger(delaySeconds)) return null;
    resolved = nowMs + delaySeconds * 1000;
  }
  const lead = resolved - nowMs;
  if (!Number.isSafeInteger(resolved) || lead < REMINDER_MIN_LEAD_MS || lead > REMINDER_MAX_LEAD_MS) return null;
  return resolved;
}

// One column holds the AES-256-GCM record from lib/crypto.js. Format (all standard base64):
//   v1:<nonce>:<ciphertext>:<tag>
export function packEndpointCiphertext(record) {
  return `v1:${record.nonce}:${record.ciphertext}:${record.tag}`;
}

export function unpackEndpointCiphertext(value) {
  const parts = String(value || "").split(":");
  if (parts.length !== 4 || parts[0] !== "v1") throw new Error("unsupported endpoint ciphertext format");
  return { nonce: parts[1], ciphertext: parts[2], tag: parts[3] };
}

export function createPushStore({
  db,
  config = WEB_PUSH,
  now = sqlNow,
  clock = () => Date.now(),
  newId = randomUUID
} = {}) {
  if (!db) throw new Error("createPushStore requires a database");
  const enabled = Boolean(config?.enabled);
  const endpointHosts = config?.endpointHosts || [];

  const subscriptionBySha = db.prepare("SELECT id, org, viewer_email FROM push_subscriptions WHERE endpoint_sha256 = ?");
  const insertSubscription = db.prepare(`INSERT INTO push_subscriptions
    (id, org, viewer_email, endpoint_ciphertext, endpoint_sha256, p256dh, auth, label, created_at)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`);
  const moveSubscription = db.prepare(`UPDATE push_subscriptions
    SET org = ?, viewer_email = ?, endpoint_ciphertext = ?, p256dh = ?, auth = ?, label = ?, failure_count = 0
    WHERE id = ?`);
  const viewerSubscriptions = db.prepare("SELECT id FROM push_subscriptions WHERE viewer_email = ? ORDER BY created_at ASC, rowid ASC");
  const deleteSubscriptionById = db.prepare("DELETE FROM push_subscriptions WHERE id = ?");
  const deleteViewerSubscription = db.prepare("DELETE FROM push_subscriptions WHERE endpoint_sha256 = ? AND viewer_email = ?");
  // Recipients are computed against the artifact's current org; administrators can read every org.
  const deviceCount = db.prepare("SELECT COUNT(*) AS count FROM push_subscriptions WHERE viewer_email = ? AND (org = ? OR org = 'admin')");
  const refreshOrg = db.prepare("UPDATE push_subscriptions SET org = ? WHERE viewer_email = ? AND org <> ?");
  const deletePendingDeliveries = db.prepare("DELETE FROM push_deliveries WHERE subscription_id = ? AND state = 'pending'");
  const optinRow = db.prepare("SELECT 1 FROM artifact_push_optins WHERE artifact_id = ? AND viewer_email = ?");
  const insertOptin = db.prepare(`INSERT INTO artifact_push_optins (artifact_id, viewer_email, org, created_at)
    VALUES (?, ?, ?, ?) ON CONFLICT(artifact_id, viewer_email) DO UPDATE SET org = excluded.org`);
  const deleteOptin = db.prepare("DELETE FROM artifact_push_optins WHERE artifact_id = ? AND viewer_email = ?");
  const listArmed = db.prepare(`SELECT key, scope, fire_at, title, body, revision FROM artifact_reminders
    WHERE artifact_id = ? AND scope = ? AND owner = ? AND state = 'armed' ORDER BY key`);
  const reminderRow = db.prepare("SELECT state, revision FROM artifact_reminders WHERE artifact_id = ? AND scope = ? AND owner = ? AND key = ?");
  const armedCount = db.prepare("SELECT COUNT(*) AS count FROM artifact_reminders WHERE artifact_id = ? AND state = 'armed'");
  const insertReminder = db.prepare(`INSERT INTO artifact_reminders
    (artifact_id, scope, owner, key, org, fire_at, title, body, state, revision, created_by, updated_at, fired_at)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'armed', 1, ?, ?, NULL)`);
  const updateReminder = db.prepare(`UPDATE artifact_reminders
    SET org = ?, fire_at = ?, title = ?, body = ?, state = 'armed', revision = revision + 1, created_by = ?, updated_at = ?, fired_at = NULL
    WHERE artifact_id = ? AND scope = ? AND owner = ? AND key = ?`);
  const deleteReminder = db.prepare("DELETE FROM artifact_reminders WHERE artifact_id = ? AND scope = ? AND owner = ? AND key = ?");

  function owner(scope, viewerEmail) {
    return scope === "viewer" ? normalizeEmail(viewerEmail) : "";
  }

  function endpointSha256(endpoint) {
    return createHash("sha256").update(endpoint, "utf8").digest("hex");
  }

  function saveSubscription({ org, viewerEmail, endpoint, keys, label = "" }) {
    if (!validPushEndpoint(endpoint, endpointHosts)) return { ok: false, error: "bad_endpoint" };
    if (!validPushKeys(keys)) return { ok: false, error: "bad_keys" };
    const email = normalizeEmail(viewerEmail);
    const sha = endpointSha256(endpoint);
    const ciphertext = packEndpointCiphertext(encrypt(endpoint, config.encryptionKey));
    const timestamp = now();
    return db.transaction(() => {
      const existing = subscriptionBySha.get(sha);
      let id;
      if (existing) {
        id = existing.id;
        // A device that changes hands must not receive the previous owner's queued notifications.
        if (existing.viewer_email !== email || existing.org !== String(org || "")) deletePendingDeliveries.run(id);
        moveSubscription.run(String(org || ""), email, ciphertext, keys.p256dh, keys.auth, label, id);
      } else {
        id = newId();
        insertSubscription.run(id, String(org || ""), email, ciphertext, sha, keys.p256dh, keys.auth, label, timestamp);
      }
      const rows = viewerSubscriptions.all(email).filter((row) => row.id !== id);
      for (const row of rows.slice(0, Math.max(0, rows.length - (PUSH_SUBSCRIPTIONS_PER_VIEWER - 1)))) {
        deleteSubscriptionById.run(row.id);
      }
      return { ok: true, id };
    }).immediate();
  }

  function removeSubscription({ viewerEmail, endpoint }) {
    if (typeof endpoint !== "string" || !endpoint) return false;
    return deleteViewerSubscription.run(endpointSha256(endpoint), normalizeEmail(viewerEmail)).changes > 0;
  }

  function decryptEndpoint(row) {
    return decrypt(unpackEndpointCiphertext(row.endpoint_ciphertext), config.encryptionKey);
  }

  function devices(org, viewerEmail) {
    return deviceCount.get(normalizeEmail(viewerEmail), String(org || "")).count;
  }

  /** Move the viewer's subscriptions to their current resolved org. Returns the changed row count. */
  function refreshViewerOrg(viewerEmail, org) {
    const email = normalizeEmail(viewerEmail);
    const current = String(org || "");
    if (!email || !current) return 0;
    return refreshOrg.run(current, email, current).changes;
  }

  function optedIn(artifactId, viewerEmail) {
    return Boolean(optinRow.get(artifactId, normalizeEmail(viewerEmail)));
  }

  function optIn(artifactId, org, viewerEmail) {
    insertOptin.run(artifactId, normalizeEmail(viewerEmail), String(org || ""), now());
  }

  function optOut(artifactId, viewerEmail) {
    deleteOptin.run(artifactId, normalizeEmail(viewerEmail));
  }

  function listReminders(artifactId, scope = "org", viewerEmail = "") {
    return listArmed.all(artifactId, scope, owner(scope, viewerEmail));
  }

  /**
   * Arm or re-arm one reminder. Returns { ok: true, key, scope, fire_at, revision } or
   * { ok: false, status, error } with the shared route/MCP error codes.
   */
  function setReminder({ artifactId, org, scope = "org", viewerEmail = "", key, fireAt, delaySeconds, title, body, createdBy }) {
    if (scope !== "org" && scope !== "viewer") return { ok: false, status: 400, error: "bad_scope" };
    if (typeof key !== "string" || !REMINDER_KEY_RE.test(key)) return { ok: false, status: 400, error: "bad_key" };
    const resolved = reminderFireAt({ fireAt, delaySeconds }, clock());
    if (resolved === null) return { ok: false, status: 400, error: "bad_time" };
    const text = reminderText(title, body);
    if (!text) return { ok: false, status: 400, error: "bad_text" };
    const rowOwner = owner(scope, viewerEmail);
    if (scope === "viewer" && !rowOwner) return { ok: false, status: 400, error: "bad_scope" };
    const timestamp = now();
    return db.transaction(() => {
      const current = reminderRow.get(artifactId, scope, rowOwner, key);
      // A key that is already armed never counts as a new row. A new key, or a fired key that is
      // re-armed, needs room under the per-artifact armed limit.
      if (current?.state !== "armed" && armedCount.get(artifactId).count >= REMINDER_ARMED_LIMIT) {
        return { ok: false, status: 409, error: "reminder_limit" };
      }
      if (current) {
        updateReminder.run(String(org || ""), resolved, text.title, text.body, createdBy, timestamp, artifactId, scope, rowOwner, key);
        return { ok: true, key, scope, fire_at: resolved, revision: current.revision + 1 };
      }
      insertReminder.run(artifactId, scope, rowOwner, key, String(org || ""), resolved, text.title, text.body, createdBy, timestamp);
      return { ok: true, key, scope, fire_at: resolved, revision: 1 };
    }).immediate();
  }

  function clearReminder(artifactId, scope, viewerEmail, key) {
    return deleteReminder.run(artifactId, scope, owner(scope, viewerEmail), key).changes > 0;
  }

  return {
    enabled,
    vapidPublicKey: enabled ? config.vapidPublicKey : null,
    endpointHosts,
    validEndpoint: (endpoint) => validPushEndpoint(endpoint, endpointHosts),
    saveSubscription,
    removeSubscription,
    decryptEndpoint,
    devices,
    refreshViewerOrg,
    optedIn,
    optIn,
    optOut,
    listReminders,
    setReminder,
    clearReminder
  };
}
