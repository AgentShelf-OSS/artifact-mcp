// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
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
