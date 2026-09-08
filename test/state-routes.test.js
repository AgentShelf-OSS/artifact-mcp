import test from "node:test";
import assert from "node:assert/strict";
import Database from "better-sqlite3";
import { IncomingMessage, ServerResponse } from "node:http";
import { Duplex } from "node:stream";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { migrateDatabase } from "../lib/migrations.js";
import { createApp } from "../lib/app.js";
import { createAuditLedger } from "../lib/audit.js";
const importDir = mkdtempSync(join(tmpdir(), "artifact-state-routes-"));
process.env.DATA_DIR = importDir;
const { createStateStore } = await import("../lib/state.js");
const { default: defaultDb } = await import("../lib/db.js");
const databases = new Set();
test.afterEach(() => { for (const db of databases) db.close(); databases.clear(); });
test.after(() => { defaultDb.close(); rmSync(importDir, { recursive: true, force: true }); });

// Run the complete Express middleware/router with real HTTP request/response streams.
// No listener is needed, so this exercises body parsing and authenticity in the sandbox.
function invoke(app, method, path, { params = {}, body, rawBody, headers = {} } = {}) {
  const url = path.replace(/:([a-z]+)/g, (_match, key) => encodeURIComponent(params[key]));
  const payload = rawBody ?? (body === undefined ? "" : JSON.stringify(body));
  const socket = new Duplex({ read() {}, write(_chunk, _encoding, callback) { callback(); } });
  const req = new IncomingMessage(socket);
  req.method = method.toUpperCase(); req.url = url;
  req.headers = { cookie: "test-session", "content-type": "application/json", "content-length": String(Buffer.byteLength(payload)), "x-artifact-mutation": "1", "sec-fetch-site": "same-origin", ...headers };
  const res = new ServerResponse(req);
  return new Promise((resolve, reject) => {
    res.end = function(chunk) {
      const text = chunk == null ? "" : chunk.toString();
      let value; try { value = JSON.parse(text); } catch { value = text || undefined; }
      resolve({ status: res.statusCode, body: value }); socket.destroy(); return res;
    };
    req.on("error", reject);
    req.push(payload || null); if (payload) req.push(null);
    app.handle(req, res, error => reject(error || new Error("unhandled test route")));
  });
}

function fixture({ viewer = { email: "viewer@acme.test", org: "acme", isAdmin: false }, limit, audit } = {}) {
  const database = new Database(":memory:");
  databases.add(database);
  database.pragma("foreign_keys = ON");
  migrateDatabase(database);
  database.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES ('owned', 'publisher', 'acme', 'Owned')").run();
  database.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES ('foreign', 'publisher', 'beta', 'Foreign')").run();
  const state = createStateStore({ db: database, now: () => "now" });
  const app = createApp({
    state,
    limits: { statePerWindow: limit ?? 1000 },
    audit,
    resolveViewer: async (req) => ({ ...viewer, email: req.headers["test-viewer-email"] ?? viewer.email }),
    artifacts: { getArtifactMeta: (id) => id === "owned" ? { id, org: "acme" } : id === "foreign" ? { id, org: "beta" } : null },
    feedback: { listForArtifact: () => [] },
    pages: { notFound: () => "not found", notSignedIn: () => "not signed in", gallery: () => "", shell: () => "", settings: () => "" },
    logger: { error() {}, warn() {} }
  });
  return { app, database, state };
}

test("state routes conceal foreign and missing artifacts", async () => {
  const { app } = fixture();
  for (const id of ["foreign", "missing"]) {
    for (const [method, path, params, body] of [["get", "/:id/state", { id }, undefined], ["get", "/:id/state/:key", { id, key: "x" }, undefined], ["put", "/:id/state/:key", { id, key: "x" }, { value: 1 }], ["delete", "/:id/state/:key", { id, key: "x" }, undefined]]) {
      const result = await invoke(app, method, path, { params, body });
      assert.equal(result.status, 404);
    }
  }
  const shared = await invoke(app, "get", "/:id/state", { params: { id: "share-token" } });
  assert.equal(shared.status, 404);
});

test("state mutations reject cross-site and null-origin viewer requests", async () => {
  const { app } = fixture();
  for (const headers of [{ "sec-fetch-site": "cross-site" }, { "sec-fetch-site": "none", origin: "null" }]) {
    const result = await invoke(app, "put", "/:id/state/:key", { params: { id: "owned", key: "note" }, body: { value: "denied" }, headers });
    assert.equal(result.status, 403);
  }
});

test("admins and a second viewer in the organization can access state while unsigned viewers cannot", async () => {
  const seeded = fixture();
  let result = await invoke(seeded.app, "put", "/:id/state/:key", { params: { id: "owned", key: "shared" }, body: { value: "yes" } });
  assert.equal(result.status, 200);
  result = await invoke(seeded.app, "get", "/:id/state/:key", { params: { id: "owned", key: "shared" }, headers: { "test-viewer-email": "second@acme.test" } });
  assert.equal(result.status, 200);
  assert.equal(result.body.value, "yes");
  assert.equal(result.body.updated_by, "viewer@acme.test");
  const admin = fixture({ viewer: { email: "admin@example.test", org: "admin", isAdmin: true } });
  result = await invoke(admin.app, "put", "/:id/state/:key", { params: { id: "foreign", key: "admin" }, body: { value: true } });
  assert.equal(result.status, 200);
  result = await invoke(admin.app, "get", "/:id/state/:key", { params: { id: "foreign", key: "admin" } });
  assert.equal(result.status, 200);
  assert.equal(result.body.value, true);
  assert.equal(result.body.updated_by, "admin@example.test");
  result = await invoke(fixture({ viewer: { email: "", org: "", isAdmin: false } }).app, "get", "/:id/state", { params: { id: "owned" } });
  assert.equal(result.status, 404);
});

test("state routes support round trip, revision conflict, strict bodies, and deletion", async () => {
  const { app } = fixture();
  let result = await invoke(app, "put", "/:id/state/:key", { params: { id: "owned", key: "note" }, body: { value: { n: 1 } } });
  assert.deepEqual(result.body, { key: "note", revision: 1, updated_at: "now" });
  result = await invoke(app, "get", "/:id/state/:key", { params: { id: "owned", key: "note" } });
  assert.equal(result.body.value.n, 1);
  result = await invoke(app, "put", "/:id/state/:key", { params: { id: "owned", key: "note" }, body: { value: 2, if_revision: 1 } });
  assert.equal(result.body.revision, 2);
  result = await invoke(app, "put", "/:id/state/:key", { params: { id: "owned", key: "note" }, body: { value: 3, if_revision: 1 } });
  assert.deepEqual(result.body, { error: "conflict", value: 2, revision: 2 });
  assert.equal(result.status, 409);
  for (const body of [{ value: 1, extra: true }, { value: 1, if_revision: null }, null, []]) {
    result = await invoke(app, "put", "/:id/state/:key", { params: { id: "owned", key: "note" }, body });
    assert.equal(result.status, 400);
  }
  result = await invoke(app, "delete", "/:id/state/:key", { params: { id: "owned", key: "note" } });
  assert.equal(result.status, 204);
});

test("state routes enforce key validation, key cap, rate class, and audit", async () => {
  const events = [];
  const { app } = fixture({ limit: 2, audit: { append: (_context, event) => events.push(event) } });
  let result = await invoke(app, "put", "/:id/state/:key", { params: { id: "owned", key: "bad key" }, body: { value: 1 } });
  assert.equal(result.status, 400);
  result = await invoke(app, "get", "/:id/state", { params: { id: "owned" } });
  assert.equal(result.status, 200);
  result = await invoke(app, "get", "/:id/state", { params: { id: "owned" } });
  assert.equal(result.status, 429);
  const cap = fixture();
  for (const key of ["ends\n", "ends\r", "ends\u2028"]) {
    result = await invoke(cap.app, "put", "/:id/state/:key", { params: { id: "owned", key }, body: { value: 1 } });
    assert.deepEqual(result, { status: 400, body: { error: "bad_key" } });
  }
  for (let i = 0; i < 64; i += 1) {
    result = await invoke(cap.app, "put", "/:id/state/:key", { params: { id: "owned", key: `k${i}` }, body: { value: i } });
    assert.equal(result.status, 200);
  }
  result = await invoke(cap.app, "put", "/:id/state/:key", { params: { id: "owned", key: "overflow" }, body: { value: 1 } });
  assert.equal(result.status, 409);
  result = await invoke(cap.app, "delete", "/:id/state/:key", { params: { id: "owned", key: "k0" } });
  assert.equal(result.status, 204);
  result = await invoke(cap.app, "put", "/:id/state/:key", { params: { id: "owned", key: "free" }, body: { value: 1 } });
  assert.equal(result.status, 200);
  assert.equal(events.length, 0);
});

test("state routes enforce the serialized UTF-8 value cap and audit deletes", async () => {
  const events = [];
  const { app } = fixture({ audit: { append: (_context, event) => events.push(event) } });
  let result = await invoke(app, "put", "/:id/state/:key", { params: { id: "owned", key: "bytes" }, body: { value: "é".repeat(128 * 1024 - 1) } });
  assert.equal(result.status, 200);
  result = await invoke(app, "put", "/:id/state/:key", { params: { id: "owned", key: "bytes" }, body: { value: "é".repeat(128 * 1024) } });
  assert.equal(result.status, 413);
  result = await invoke(app, "delete", "/:id/state/:key", { params: { id: "owned", key: "bytes" } });
  assert.equal(result.status, 204);
  assert.equal(events.length, 1);
  assert.equal(events[0].operation, "state.delete");
  assert.equal(events[0].targetId, "owned");
});

test("deletes are accepted by the real audit ledger for digit-leading artifact ids and mixed-case keys", async () => {
  const database = new Database(":memory:");
  databases.add(database);
  database.pragma("foreign_keys = ON");
  migrateDatabase(database);
  database.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES ('0abc12def345', 'publisher', 'acme', 'Digit')").run();
  const state = createStateStore({ db: database, now: () => "now" });
  const audit = createAuditLedger({ db: database, hmacKey: Buffer.alloc(32, 7).toString("base64") });
  const app = createApp({
    state,
    limits: { statePerWindow: 1000 },
    audit,
    resolveViewer: async () => ({ email: "viewer@acme.test", org: "acme", isAdmin: false }),
    artifacts: { getArtifactMeta: (id) => id === "0abc12def345" ? { id, org: "acme" } : null },
    feedback: { listForArtifact: () => [] },
    pages: { notFound: () => "not found", notSignedIn: () => "not signed in", gallery: () => "", shell: () => "", settings: () => "" },
    logger: { error() {}, warn() {} }
  });
  let result = await invoke(app, "put", "/:id/state/:key", { params: { id: "0abc12def345", key: "Reading.Notes-1" }, body: { value: 1 } });
  assert.equal(result.status, 200);
  result = await invoke(app, "delete", "/:id/state/:key", { params: { id: "0abc12def345", key: "Reading.Notes-1" } });
  assert.equal(result.status, 204);
  const row = database.prepare("SELECT target_type, target_id FROM security_audit_events WHERE operation = 'state.delete'").get();
  assert.deepEqual(row, { target_type: "artifact_state", target_id: "0abc12def345" });
});

test("an audit failure rolls the state deletion back", async () => {
  const { app, state } = fixture({ audit: { append() { throw new Error("audit unavailable"); } } });
  state.put("owned", "keep", "important", "viewer@acme.test");
  const result = await invoke(app, "delete", "/:id/state/:key", { params: { id: "owned", key: "keep" } });
  assert.equal(result.status, 500);
  assert.equal(state.get("owned", "keep").value, "important");
});
