// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import { createECDH } from "node:crypto";
import { parseEncryptionKey } from "./crypto.js";

function positiveInteger(value, fallback) {
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) && parsed > 0 ? parsed : fallback;
}

function nonEmptyString(value, fallback) {
  const normalized = String(value || "").trim();
  return normalized || fallback;
}

export function mcpJsonLimitFor(maxBundleBytes) {
  // A one-byte control character expands to a six-byte \u00XX JSON escape. Reserve
  // a fixed envelope for JSON-RPC metadata, file names, titles, and descriptions too.
  return maxBundleBytes * 6 + 256 * 1024;
}

export const APP_NAME = nonEmptyString(process.env.APP_NAME, "Artifact Index");
export const APP_BRAND = nonEmptyString(process.env.APP_BRAND, "A");
export function parseArtifactCastGrants(value = "") {
  const grants = new Set();
  const raw = String(value).trim();
  if (!raw) return grants;
  for (const grant of raw.split(",").map((part) => part.trim())) {
    const match = /^([0-9a-z]{6,24})@([1-9][0-9]*)$/.exec(grant);
    if (!match || !Number.isSafeInteger(Number(match[2]))) {
      throw new Error("ARTIFACT_CAST_IDS must be a comma-separated list of artifactId@positiveRevision grants");
    }
    grants.add(`${match[1]}@${Number(match[2])}`);
  }
  return grants;
}
export const ARTIFACT_CAST_IDS = parseArtifactCastGrants(process.env.ARTIFACT_CAST_IDS);
export const FEEDBACK_MAX_BODY = positiveInteger(process.env.FEEDBACK_MAX_BODY, 4000);
// How many past revisions to retain per artifact (older snapshots are pruned).
export const MAX_HISTORY = positiveInteger(process.env.MAX_HISTORY, 20);
export const MAX_ARTIFACT_BYTES = positiveInteger(process.env.MAX_ARTIFACT_BYTES, 2 * 1024 * 1024);
export const MAX_BUNDLE_BYTES = positiveInteger(process.env.MAX_BUNDLE_BYTES, 8 * 1024 * 1024);
export const MAX_BUNDLE_FILES = positiveInteger(process.env.MAX_BUNDLE_FILES, 100);
export const MCP_JSON_LIMIT = process.env.MCP_JSON_LIMIT || mcpJsonLimitFor(MAX_BUNDLE_BYTES);
export const STATE_MAX_KEY_LENGTH = 64;
export const STATE_MAX_VALUE_BYTES = 256 * 1024;
export const STATE_MAX_KEYS = 64;
// State requests get a small JSON envelope allowance over the serialized value cap.
export const STATE_JSON_LIMIT = "257kb";
export const STATE_PER_WINDOW = positiveInteger(process.env.INGRESS_STATE_PER_WINDOW, 30);
export const STATE_RATE_WINDOW_MS = positiveInteger(process.env.INGRESS_RATE_WINDOW_SECONDS, 60) * 1000;

// ADR-0012 Web Push reminders. Node stores reminders but never sends them; Rust owns delivery.
export const DEFAULT_WEB_PUSH_ENDPOINT_HOSTS = Object.freeze([
  "fcm.googleapis.com",
  "updates.push.services.mozilla.com",
  "push.services.mozilla.com",
  "web.push.apple.com",
  "*.push.apple.com",
  "*.notify.windows.com"
]);
const WEB_PUSH_HOST_ENTRY_RE = /^(\*\.)?(?=.{1,253}$)[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)*$/;

export function parseWebPushEndpointHosts(value) {
  const raw = String(value ?? "").trim();
  if (!raw) return [...DEFAULT_WEB_PUSH_ENDPOINT_HOSTS];
  const hosts = [];
  for (const entry of raw.split(",").map((part) => part.trim().toLowerCase()).filter(Boolean)) {
    const labels = (entry.startsWith("*.") ? entry.slice(2) : entry).split(".");
    // At least two labels (no bare TLD or "*.com"), and no all-numeric label, so an entry can never
    // name an IPv4 literal. IPv6 literals fail the character class.
    if (!WEB_PUSH_HOST_ENTRY_RE.test(entry) || labels.length < 2 || labels.some((label) => /^[0-9]+$/.test(label))) {
      throw new Error("WEB_PUSH_ENDPOINT_HOSTS must be a comma-separated list of host names or *.suffix entries (at least two labels, no IP literals or all-numeric labels)");
    }
    if (!hosts.includes(entry)) hosts.push(entry);
  }
  if (!hosts.length) return [...DEFAULT_WEB_PUSH_ENDPOINT_HOSTS];
  return hosts;
}

export function parseVapidPrivateKey(value) {
  const encoded = String(value ?? "").trim();
  const error = new Error("WEB_PUSH_VAPID_PRIVATE_KEY must be the base64url (no padding) encoding of a 32-byte P-256 private key");
  if (!/^[A-Za-z0-9_-]{43}$/.test(encoded)) throw error;
  const scalar = Buffer.from(encoded, "base64url");
  if (scalar.length !== 32 || scalar.toString("base64url") !== encoded) throw error;
  const ecdh = createECDH("prime256v1");
  try { ecdh.setPrivateKey(scalar); } catch { throw error; }
  return { privateKey: scalar, publicKey: ecdh.getPublicKey(null, "uncompressed").toString("base64url") };
}

export function validWebPushSubject(value) {
  const subject = String(value ?? "");
  if (!subject || subject.length > 256 || /[\s\u0000-\u001f\u007f]/u.test(subject)) return false;
  if (subject.startsWith("mailto:")) return subject.length > "mailto:".length;
  if (!subject.startsWith("https:")) return false;
  try {
    const url = new URL(subject);
    return url.protocol === "https:" && Boolean(url.hostname) && !url.username && !url.password;
  } catch {
    return false;
  }
}

/**
 * Parse the Web Push configuration. An unset key disables the feature. A set key must be valid
 * and must come with a valid subject; otherwise startup fails like every other config error.
 * The feature is enabled only when WEBHOOK_ENC_KEY is also configured, because endpoints are
 * stored encrypted with that key.
 */
export function parseWebPushConfig(env = process.env) {
  const endpointHosts = parseWebPushEndpointHosts(env.WEB_PUSH_ENDPOINT_HOSTS);
  const rawKey = String(env.WEB_PUSH_VAPID_PRIVATE_KEY ?? "").trim();
  const disabled = { enabled: false, vapidPublicKey: null, vapidPrivateKey: null, subject: null, endpointHosts, encryptionKey: null };
  if (!rawKey) return disabled;
  const { privateKey, publicKey } = parseVapidPrivateKey(rawKey);
  const subject = String(env.WEB_PUSH_SUBJECT ?? "").trim();
  if (!validWebPushSubject(subject)) {
    throw new Error("WEB_PUSH_SUBJECT must be a mailto: or https: contact URL when WEB_PUSH_VAPID_PRIVATE_KEY is set");
  }
  const encryptionKey = String(env.WEBHOOK_ENC_KEY ?? "").trim();
  if (!encryptionKey || !parseEncryptionKey(encryptionKey)) return disabled;
  return { enabled: true, vapidPublicKey: publicKey, vapidPrivateKey: privateKey, subject, endpointHosts, encryptionKey };
}

export const WEB_PUSH = parseWebPushConfig(process.env);
