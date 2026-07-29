// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// Comparison modes from blueprint B3. Each mode is a pure "normalizer": it turns a raw
// observation (response bytes, header map, SQL rows, files) into a canonical, JSON-
// serializable form. That SAME form is what `--record` writes as the golden and what a
// replay compares against — so recording and checking can never drift apart.
//
// Modes:
//   exact-bytes     Raw artifacts, downloads, static assets, selected HTML snapshots.
//   exact-header    Header names lowercased; transport-only headers stripped; values exact.
//   canonical-json  Recursive key-sort; array order significant.
//   exact-json-text MCP content[0].text embedded JSON + tools/list golden (exact string).
//   state           Named SQL query rows + directory entries + sha256 file hashes.
//   html-dom        Large trusted pages: exact snapshot primary (DOM checks are secondary).

import { createHash } from "node:crypto";

export const BODY_MODES = new Set([
  "exact-bytes",
  "canonical-json",
  "exact-json-text",
  "html-dom"
]);

// Transport-only headers are removed before comparison: they vary per-connection/per-run
// and say nothing about application behavior.
export const TRANSPORT_HEADERS = new Set([
  "date",
  "server",
  "content-length",
  "connection",
  "keep-alive",
  "transfer-encoding"
]);

export function sha256Hex(buf) {
  return createHash("sha256").update(buf).digest("hex");
}

// --- Canonical JSON --------------------------------------------------------------------
export function canonicalize(value) {
  if (Array.isArray(value)) return value.map(canonicalize);
  if (value && typeof value === "object") {
    const out = {};
    for (const key of Object.keys(value).sort()) out[key] = canonicalize(value[key]);
    return out;
  }
  return value;
}

export function stableStringify(value) {
  return JSON.stringify(canonicalize(value));
}

// Replace every property whose KEY appears in `volatileFields`, anywhere in the tree, with
// a fixed sentinel. This is how a case declares "this timestamp is allowed to move".
function blankVolatile(value, volatileFields) {
  if (!volatileFields || volatileFields.length === 0) return value;
  const set = new Set(volatileFields);
  // MCP embeds its payload as JSON *inside* a string (`result.content[].text`). A tree walk sees
  // that as an opaque leaf, so a volatile timestamp in there survives and makes the golden
  // unreproducible. Re-serializing the inner JSON would destroy the byte-exactness that
  // `exact-json-text` exists to protect, so substitute the value in place instead and leave every
  // other byte — including key order and spacing — untouched.
  const blankInsideEmbeddedJson = (text) => {
    let out = text;
    for (const field of set) {
      out = out.replaceAll(
        new RegExp(`("${field}"\\s*:\\s*)"[^"\\\\]*"`, "g"),
        '$1"<volatile>"'
      );
    }
    return out;
  };
  const walk = (node) => {
    if (Array.isArray(node)) return node.map(walk);
    if (node && typeof node === "object") {
      const out = {};
      for (const [k, v] of Object.entries(node)) out[k] = set.has(k) ? "<volatile>" : walk(v);
      return out;
    }
    if (typeof node === "string" && node.includes('":')) return blankInsideEmbeddedJson(node);
    return node;
  };
  return walk(value);
}

// Back-substitute high-entropy captured values (artifact ids, share tokens) with their
// symbol name so goldens are run-independent. Only applied to captures, never to the
// low-entropy constants (org names, emails) which stay literal.
export function backsubString(str, captures) {
  let out = str;
  // Longest values first so a value that is a prefix of another cannot mis-replace.
  const entries = Object.entries(captures || {}).sort((a, b) => String(b[1]).length - String(a[1]).length);
  for (const [name, val] of entries) {
    if (!val) continue;
    out = out.split(String(val)).join("${" + name + "}");
  }
  return out;
}

// --- Body normalizers ------------------------------------------------------------------
// Each returns a JSON-serializable "normal form" for storage/compare, or throws on a hard
// shape error (e.g. body is not JSON when JSON was expected).
export function normalizeBody(mode, bodyBuf, { captures = {}, volatileFields = [] } = {}) {
  switch (mode) {
    case "canonical-json":
    case "exact-json-text": {
      const text = bodyBuf.toString("utf8");
      let parsed;
      try {
        parsed = JSON.parse(text);
      } catch (err) {
        throw new Error(`expected JSON body for mode ${mode} but parse failed: ${err.message}`);
      }
      const blanked = blankVolatile(parsed, volatileFields);
      // Canonicalize, then back-substitute captured ids/tokens inside string leaves.
      const canonicalText = backsubString(stableStringify(blanked), captures);
      const normal = { mode, json: JSON.parse(canonicalText) };
      if (mode === "exact-json-text") {
        // Assert every MCP content[].text that looks like JSON is valid JSON (the embedded
        // contract). Purely a shape guard; the value comparison is the canonical json above.
        assertEmbeddedTextJson(parsed);
      }
      return normal;
    }
    case "exact-bytes":
    case "html-dom": {
      // Prefer readable utf8 when the body is text; fall back to base64 for binary (png).
      const isText = looksTextual(bodyBuf);
      if (isText) {
        return { mode, encoding: "utf8", data: backsubString(bodyBuf.toString("utf8"), captures) };
      }
      return { mode, encoding: "base64", sha256: sha256Hex(bodyBuf), data: bodyBuf.toString("base64") };
    }
    default:
      throw new Error(`unknown body mode: ${mode}`);
  }
}

function looksTextual(buf) {
  // Treat as text if it decodes as utf8 without replacement chars and has no NUL bytes.
  if (buf.includes(0)) return false;
  const text = buf.toString("utf8");
  return !text.includes("�");
}

function assertEmbeddedTextJson(parsed) {
  const check = (result) => {
    if (result && Array.isArray(result.content)) {
      for (const part of result.content) {
        if (part && part.type === "text" && typeof part.text === "string") {
          const t = part.text.trim();
          if (t.startsWith("{") || t.startsWith("[")) {
            JSON.parse(part.text); // throws if the embedded contract is not valid JSON
          }
        }
      }
    }
  };
  const messages = Array.isArray(parsed) ? parsed : [parsed];
  for (const msg of messages) if (msg && msg.result) check(msg.result);
}

// --- Header normalizer -----------------------------------------------------------------
export function normalizeHeaders(rawHeaders, { captures = {} } = {}) {
  const out = {};
  for (const [name, value] of Object.entries(rawHeaders || {})) {
    const lower = name.toLowerCase();
    if (TRANSPORT_HEADERS.has(lower)) continue;
    const flat = Array.isArray(value) ? value.join(", ") : String(value);
    out[lower] = backsubString(flat, captures);
  }
  // Sorted object for stable serialization.
  const sorted = {};
  for (const key of Object.keys(out).sort()) sorted[key] = out[key];
  return { mode: "exact-header", headers: sorted };
}

// --- Structural compare ----------------------------------------------------------------
export function deepEqual(a, b) {
  return stableStringify(a) === stableStringify(b);
}

// Produce a compact human-readable diff for a failed comparison.
export function describeDiff(expected, actual) {
  return (
    "  expected: " + JSON.stringify(canonicalize(expected)) + "\n" +
    "  actual:   " + JSON.stringify(canonicalize(actual))
  );
}
