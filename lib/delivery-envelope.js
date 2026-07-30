// SPDX-License-Identifier: Apache-2.0
// Canonical, secret-free Discord outbox planning. No transport is performed here.
import crypto from "node:crypto";
import { buildEmbed } from "./notify.js";
import { MAX_OUTBOX_PAYLOAD_BYTES } from "./outbox.js";

const EVENTS = new Set(["published", "updated", "restored", "deleted", "feedback", "resolved"]);
const trustedEnvelopes = new WeakSet();

const isObject = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
const hasExactKeys = (value, keys) => {
  if (!isObject(value)) return false;
  const actual = Object.keys(value);
  return actual.length === keys.length && keys.every((key, index) => actual[index] === key);
};
const nonEmpty = (value) => typeof value === "string" && value.trim().length > 0;
function deepFreeze(value) {
  if (isObject(value) || Array.isArray(value)) {
    for (const child of Object.values(value)) deepFreeze(child);
    Object.freeze(value);
  }
  return value;
}
function invalid() { throw new TypeError("invalid delivery envelope"); }
function assertDiscordPayload(payload) {
  if (!hasExactKeys(payload, ["embeds"]) || !Array.isArray(payload.embeds) || !payload.embeds.length) invalid();
  for (const embed of payload.embeds) {
    const optional = Object.hasOwn(embed, "url") ? ["url"] : [];
    if (!hasExactKeys(embed, ["color", "author", "title", ...optional, "fields", "description"]) || !Number.isInteger(embed.color) || embed.color < 0 || embed.color > 0xffffff || !hasExactKeys(embed.author, ["name"]) || typeof embed.author.name !== "string" || typeof embed.title !== "string" || (Object.hasOwn(embed, "url") && typeof embed.url !== "string") || !Array.isArray(embed.fields) || typeof embed.description !== "string" || Object.hasOwn(embed, "image")) invalid();
    for (const field of embed.fields) if (!hasExactKeys(field, ["name", "value", "inline"]) || typeof field.name !== "string" || typeof field.value !== "string" || typeof field.inline !== "boolean") invalid();
  }
}
function assertEnvelopeShape(envelope, expected = {}) {
  if (!hasExactKeys(envelope, ["version", "event_id", "tenant", "event_type", "provider", "payload"]) || envelope.version !== 1 || envelope.provider !== "discord" || !nonEmpty(envelope.event_id) || !nonEmpty(envelope.tenant) || !EVENTS.has(envelope.event_type) || (expected.tenant !== undefined && envelope.tenant !== String(expected.tenant)) || (expected.event !== undefined && envelope.event_type !== String(expected.event)) || (expected.event_id !== undefined && envelope.event_id !== String(expected.event_id))) invalid();
  assertDiscordPayload(envelope.payload);
}
function trust(envelope, expected) {
  assertEnvelopeShape(envelope, expected);
  const bytes = Buffer.from(JSON.stringify(envelope));
  if (bytes.length > MAX_OUTBOX_PAYLOAD_BYTES) throw new RangeError("delivery envelope exceeds 32 KiB");
  deepFreeze(envelope);
  trustedEnvelopes.add(envelope);
  return envelope;
}

export function stableDeliveryEventId(tenant, event, subject) {
  if (!EVENTS.has(String(event))) throw new TypeError("unknown webhook event");
  return `delivery:v1:${crypto.createHash("sha256").update(`delivery-envelope-v1\0${String(tenant)}\0${event}\0${String(subject)}`).digest("hex")}`;
}

/** Builds the only locally-trusted v1 envelope shape. */
export function buildDeliveryEnvelopeV1({ event_id, tenant, event, payload = {} } = {}) {
  const envelope = {
    version: 1,
    event_id: String(event_id || ""),
    tenant: String(tenant || ""),
    event_type: String(event),
    provider: "discord",
    payload: buildEmbed(String(event), { ...payload, org: String(tenant || "") }),
  };
  return trust(envelope, { tenant, event, event_id });
}

/** Strictly accepts one byte-for-byte canonical v1 envelope bound to a queue row. */
export function decodeDeliveryEnvelopeV1(bytes, { tenant, event, event_id, payload_sha256 } = {}) {
  const input = Buffer.from(bytes);
  if (input.length > MAX_OUTBOX_PAYLOAD_BYTES) throw new RangeError("delivery envelope exceeds 32 KiB");
  let envelope;
  try { envelope = JSON.parse(input.toString("utf8")); } catch { invalid(); }
  assertEnvelopeShape(envelope, { tenant, event, event_id });
  const canonical = Buffer.from(JSON.stringify(envelope));
  if (!canonical.equals(input)) invalid();
  if (payload_sha256 !== undefined && crypto.createHash("sha256").update(canonical).digest("hex") !== payload_sha256) invalid();
  return trust(envelope, { tenant, event, event_id });
}

/** Rejects hand-built public objects before they can be stored or scheduled. */
export function validateDeliveryEnvelopeV1(envelope, expected = {}) {
  if (!trustedEnvelopes.has(envelope)) invalid();
  assertEnvelopeShape(envelope, expected);
  return envelope;
}

export function canonicalDeliveryEnvelopeBytes(envelope) {
  validateDeliveryEnvelopeV1(envelope);
  return Buffer.from(JSON.stringify(envelope));
}

/** Canonical HTTP request bytes: the nested Discord body, never envelope metadata. */
export function discordRequestBodyBytes(envelope) {
  validateDeliveryEnvelopeV1(envelope);
  return Buffer.from(JSON.stringify(envelope.payload));
}

export const deliveryEnvelopeHash = (envelope) => crypto.createHash("sha256").update(canonicalDeliveryEnvelopeBytes(envelope)).digest("hex");
