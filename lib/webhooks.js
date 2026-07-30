// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
// Per-org Discord webhooks. Full URLs remain in this module and notify.js only.
import { customAlphabet } from "nanoid";
import db from "./db.js";
import { decrypt, encrypt, parseEncryptionKey } from "./crypto.js";
import { orgExists } from "./orgs.js";
import { isDiscordWebhookUrl } from "./discord-webhook-url.js";

export const EVENTS = ["published", "updated", "restored", "deleted", "feedback", "resolved"];

const generateId = customAlphabet("0123456789abcdefghijkmnpqrstuvwxyz", 12);
const listForOrgStmt = db.prepare("SELECT * FROM org_webhooks WHERE org = ? ORDER BY created_at ASC, id ASC");
const getStmt = db.prepare("SELECT * FROM org_webhooks WHERE id = ?");
const insertStmt = db.prepare(`
  INSERT INTO org_webhooks (id, org, url, url_cipher, url_nonce, url_tag, label, events)
  VALUES (@id, @org, @url, @url_cipher, @url_nonce, @url_tag, @label, @events)
`);
const removeStmt = db.prepare("DELETE FROM org_webhooks WHERE org = ? AND id = ?");
const eventsStmt = db.prepare("UPDATE org_webhooks SET events = ? WHERE org = ? AND id = ?");
const okStmt = db.prepare("UPDATE org_webhooks SET last_ok_at = datetime('now'), last_error = NULL WHERE id = ?");
const errorStmt = db.prepare("UPDATE org_webhooks SET last_error = ? WHERE id = ?");

function parseEvents(value, { defaultAll = false } = {}) {
  if (value === undefined || value === null) return defaultAll ? [...EVENTS] : [];
  if (!Array.isArray(value)) throw new Error("Webhook events must be an array.");
  const events = [...new Set(value.map((event) => String(event || "").trim()))];
  for (const event of events) {
    if (!EVENTS.includes(event)) throw new Error(`Unknown webhook event: ${event}`);
  }
  return events;
}

function rowEvents(row) {
  return String(row.events || "").split(",").filter((event) => EVENTS.includes(event));
}

export function maskUrl(value) {
  const url = String(value || "");
  try {
    const parsed = new URL(url);
    return `${parsed.protocol}//${parsed.host}…${url.slice(-4)}`;
  } catch {
    return `…${url.slice(-4)}`;
  }
}

function storedMaskUrl(value) {
  const url = String(value || "");
  try {
    const parsed = new URL(url);
    return `${parsed.protocol}//${parsed.host}/…${url.slice(-4)}`;
  } catch {
    return `…${url.slice(-4)}`;
  }
}

function publicRow(row) {
  if (!row) return undefined;
  return {
    id: row.id,
    label: row.label,
    events: rowEvents(row),
    url: maskUrl(row.url),
    last_ok_at: row.last_ok_at,
    last_error: row.last_error
  };
}

export function listForOrg(org) {
  return listForOrgStmt.all(String(org || "").trim()).map(publicRow);
}

// Internal: callers delivering Discord notifications need the original secret URL.
export function forEvent(org, event) {
  if (!EVENTS.includes(event)) return [];
  return listForOrgStmt.all(String(org || "").trim())
    .filter((row) => rowEvents(row).includes(event))
    .map(deliveryRow);
}

export function create({ org, url, label, events } = {}) {
  org = String(org || "").trim();
  url = String(url || "").trim();
  if (!orgExists(org)) throw new Error(`Unknown organization "${org}".`);
  if (!isDiscordWebhookUrl(url)) throw new Error("Webhook URL must be an HTTPS Discord webhook URL.");
  const key = parseEncryptionKey();
  const encrypted = key ? encrypt(url) : undefined;
  const row = {
    id: generateId(),
    org,
    url: encrypted ? storedMaskUrl(url) : url,
    url_cipher: encrypted?.ciphertext ?? null,
    url_nonce: encrypted?.nonce ?? null,
    url_tag: encrypted?.tag ?? null,
    label: String(label || "").trim().slice(0, 80),
    events: parseEvents(events, { defaultAll: true }).join(",")
  };
  insertStmt.run(row);
  return publicRow(getStmt.get(row.id));
}

export function remove(org, id) {
  return removeStmt.run(String(org || "").trim(), String(id || "").trim()).changes > 0;
}

export function setEvents(org, id, events) {
  const normalized = parseEvents(events).join(",");
  const safeOrg = String(org || "").trim();
  const safeId = String(id || "").trim();
  if (!eventsStmt.run(normalized, safeOrg, safeId).changes) return undefined;
  return publicRow(getStmt.get(safeId));
}

// Internal: never pass this return value to an HTTP renderer/response.
export function get(id) {
  return deliveryRow(getStmt.get(String(id || "").trim()));
}

function deliveryRow(row) {
  if (!row?.url_cipher) return row;
  return { ...row, url: decrypt(row) };
}

export function recordResult(id, ok, error = "") {
  if (ok) okStmt.run(String(id || ""));
  else errorStmt.run(String(error || "Webhook delivery failed.").slice(0, 500), String(id || ""));
}
