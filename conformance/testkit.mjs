// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// Shared, implementation-neutral constants and helpers for the conformance oracle.
// Nothing here talks HTTP or spawns processes — that lives in the drivers and runner.
// Node stdlib only (plus a resolved better-sqlite3 for post-state SQL).

import { existsSync, readFileSync } from "node:fs";
import { createServer } from "node:net";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
export const CONFORMANCE_DIR = HERE;
export const REPO_ROOT = resolve(HERE, "..");
export const CASES_DIR = join(HERE, "cases");
export const GOLDENS_DIR = join(HERE, "goldens");
export const FIXTURES_DIR = join(HERE, "fixtures");

// --- Well-known test credentials -------------------------------------------------------
//
// These are the ONLY publisher keys and viewer identities the oracle uses. They are seeded
// into every server (Node or Rust) through env at boot, so both implementations answer the
// same authenticated requests deterministically. Secrets are long, fixed, and non-secret
// (this is a test fixture, never a deployment).
//
// ARTIFACT_API_KEYS entry format is "clientId:org:secret" (see lib/db.js seedKeysFromEnv).
export const CREDENTIALS = {
  acme: { clientId: "pub-acme", org: "acme", secret: "conf-key-acme-000000000000" },
  acme2: { clientId: "pub-acme2", org: "acme", secret: "conf-key-acme2-11111111111" },
  beta: { clientId: "pub-beta", org: "beta", secret: "conf-key-beta-222222222222" },
  admin: { clientId: "pub-admin", org: "admin", secret: "conf-key-admin-33333333333" }
};

export const VIEWERS = {
  admin_email: "admin@corp.test",
  viewer_acme: "alice@acme.test",
  viewer_beta: "bob@beta.test"
};

// A fixed public base so response `url` fields are stable regardless of the random port.
export const PUBLIC_BASE = "http://conformance.test";

// Base environment applied to every server launch. Case-level env overrides win.
export function baseEnv() {
  const keys = Object.values(CREDENTIALS)
    .map((c) => `${c.clientId}:${c.org}:${c.secret}`)
    .join(",");
  return {
    LISTEN_HOST: "127.0.0.1",
    TRUST_ACCESS_HEADERS: "1",
    ARTIFACT_API_KEYS: keys,
    ADMIN_EMAILS: VIEWERS.admin_email,
    ORG_EMAIL_DOMAINS: "acme.test:acme,beta.test:beta",
    PUBLIC_BASE_URL: PUBLIC_BASE,
    // Fixed non-secret test key so both runtimes exercise the mandatory tamper-evident ledger.
    AUDIT_LEDGER_HMAC_KEY: Buffer.alloc(32, 7).toString("base64"),
    // Keep preview/thumbnail rendering off so no headless browser is required.
    PREVIEW_RENDERER_URL: "",
    NODE_ENV: "test"
  };
}

// Constant substitution symbols usable in case steps as ${name}. These are forward-only:
// injected into requests but never back-substituted out of responses (they are low-entropy
// and would corrupt goldens). High-entropy runtime captures (artifact ids, share tokens)
// are tracked separately and DO get back-substituted (see runner).
export function constantSymbols() {
  return {
    key_acme: CREDENTIALS.acme.secret,
    key_acme2: CREDENTIALS.acme2.secret,
    key_beta: CREDENTIALS.beta.secret,
    key_admin: CREDENTIALS.admin.secret,
    client_acme: CREDENTIALS.acme.clientId,
    client_acme2: CREDENTIALS.acme2.clientId,
    client_beta: CREDENTIALS.beta.clientId,
    admin_email: VIEWERS.admin_email,
    viewer_acme: VIEWERS.viewer_acme,
    viewer_beta: VIEWERS.viewer_beta,
    public_base: PUBLIC_BASE
  };
}

// --- Dependency resolution -------------------------------------------------------------
//
// The worktree may not carry its own node_modules. Resolve a usable one (with express +
// better-sqlite3 built for this Node) from, in order: an explicit override, the worktree,
// then the sibling main checkout. Nothing is installed or written here.
export function resolveNodeModules() {
  const candidates = [
    process.env.CONFORMANCE_NODE_MODULES,
    join(REPO_ROOT, "node_modules"),
    resolve(REPO_ROOT, "..", "..", "artifact-mcp", "node_modules")
  ].filter(Boolean);
  for (const dir of candidates) {
    if (existsSync(join(dir, "express")) && existsSync(join(dir, "better-sqlite3"))) {
      return dir;
    }
  }
  throw new Error(
    "Could not locate a node_modules with express + better-sqlite3. Checked:\n  " +
    candidates.join("\n  ") +
    "\nSet CONFORMANCE_NODE_MODULES to a built node_modules directory."
  );
}

let _Database = null;
// Dynamically load better-sqlite3 from the resolved node_modules (it is not resolvable from
// the worktree root). Used only for read-only post-state assertions.
export async function loadBetterSqlite() {
  if (_Database) return _Database;
  const nm = resolveNodeModules();
  const pkgDir = join(nm, "better-sqlite3");
  const pkg = JSON.parse(readFileSync(join(pkgDir, "package.json"), "utf8"));
  const mainFile = join(pkgDir, pkg.main || "lib/index.js");
  const mod = await import(pathToFileURL(mainFile).href);
  _Database = mod.default || mod;
  return _Database;
}

// --- Small async / net utilities -------------------------------------------------------
export function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

// Grab a free loopback TCP port by binding to 0 then releasing it.
export function freePort() {
  return new Promise((res, rej) => {
    const srv = createServer();
    srv.once("error", rej);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close((err) => (err ? rej(err) : res(port)));
    });
  });
}

// Minimal HTTP client returning { status, headers, body:Buffer }.
//
// `path` is sent VERBATIM as the request-target. It is deliberately NOT run through the
// WHATWG URL parser, which would collapse dot-segments and percent-decode %2e — that
// normalization would silently defuse the bundle path-traversal cases before they ever
// reach the server. Only the origin (host/port) is parsed out of baseUrl.
export async function httpRequest({ baseUrl, method = "GET", path = "/", headers = {}, body }) {
  const http = await import("node:http");
  const origin = new URL(baseUrl);
  const payload = body == null ? null : Buffer.isBuffer(body) ? body : Buffer.from(String(body));
  const outHeaders = { ...headers };
  if (payload && outHeaders["content-length"] === undefined) {
    outHeaders["content-length"] = String(payload.length);
  }
  return new Promise((resolvePromise, reject) => {
    const req = http.request(
      { hostname: origin.hostname, port: origin.port, path, method, headers: outHeaders },
      (res) => {
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () =>
          resolvePromise({ status: res.statusCode, headers: res.headers, body: Buffer.concat(chunks) })
        );
      }
    );
    req.on("error", reject);
    if (payload) req.write(payload);
    req.end();
  });
}

// Poll /health until it returns 200 or the deadline passes.
export async function waitForHealth(baseUrl, { timeoutMs = 15000, intervalMs = 100 } = {}) {
  const deadline = Date.now() + timeoutMs;
  let lastErr = null;
  while (Date.now() < deadline) {
    try {
      const res = await httpRequest({ baseUrl, path: "/health" });
      if (res.status === 200) return true;
      lastErr = new Error(`/health returned ${res.status}`);
    } catch (err) {
      lastErr = err;
    }
    await sleep(intervalMs);
  }
  throw new Error(`Server did not become healthy at ${baseUrl}: ${lastErr?.message || lastErr}`);
}
