import test from "node:test";
import assert from "node:assert/strict";
import { IncomingMessage, ServerResponse } from "node:http";
import { Duplex } from "node:stream";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
process.env.AUDIT_LEDGER_HMAC_KEY = Buffer.alloc(32, 7).toString("base64");
const importDir = mkdtempSync(join(tmpdir(), "artifact-library-"));
process.env.DATA_DIR = importDir;
const { createApp, libraryBody } = await import("../lib/app.js");
const { default: defaultDb } = await import("../lib/db.js");
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


// ADR-0019: the native media library lists what the gallery lists, minus hidden uploads.
const rows = [
  { id: "old", title: "Old", category: "Family", updated_at: "2026-10-01T00:00:00Z", hidden: false, owner_email: "viewer@acme.test" },
  { id: "new", title: "Road trip", category: "Family", updated_at: "2026-10-18T00:00:00Z", hidden: false, owner_email: "x@acme.test" },
  { id: "mine-hidden", title: "Draft", category: "", updated_at: "2026-10-18T01:00:00Z", hidden: true, owner_email: "viewer@acme.test" }
];

function app(viewer) {
  return createApp({
    resolveViewer: async () => viewer,
    artifacts: {
      listOrgArtifacts: (org) => org === "acme" ? rows : [],
      listAllGroupedByOrg: () => new Map([["acme", rows], ["beta", [{ id: "b1", title: "Beta", category: "", updated_at: "2026-10-02T00:00:00Z", hidden: false }]]])
    },
    orgs: { names: () => ["acme", "beta", "empty"], colorMap: () => ({ acme: "#1F5F8B", beta: "url(x)" }) },
    feedback: { listForArtifact: () => [] },
    pages: { notFound: () => "not found", notSignedIn: () => "not signed in", gallery: () => "", shell: () => "", settings: () => "" },
    logger: { error() {}, warn() {} }
  });
}

test("library lists visible artifacts newest first and never hidden uploads", async () => {
  const response = await invoke(app({ email: "viewer@acme.test", org: "acme", isAdmin: false }), "get", "/api/library");
  assert.equal(response.status, 200);
  assert.deepEqual(response.body, { orgs: [{ org: "acme", color: "#1F5F8B", artifacts: [
    { id: "new", title: "Road trip", category: "Family", updated_at: "2026-10-18T00:00:00Z" },
    { id: "old", title: "Old", category: "Family", updated_at: "2026-10-01T00:00:00Z" }
  ] }] });
});

test("administrators see every organization, including empty ones", async () => {
  const response = await invoke(app({ email: "admin@acme.test", org: "acme", isAdmin: true }), "get", "/api/library");
  assert.deepEqual(response.body.orgs.map(o => [o.org, o.color, o.artifacts.map(a => a.id)]), [["acme", "#1F5F8B", ["new", "old"]], ["beta", null, ["b1"]], ["empty", null, []]]);
});

test("signed-out viewers are refused", async () => {
  const response = await invoke(app({ email: null, org: null, isAdmin: false }), "get", "/api/library");
  assert.equal(response.status, 403);
  assert.deepEqual(response.body, { error: "not_signed_in" });
});

test("library caps each organization at 200 artifacts", () => {
  const many = Array.from({ length: 250 }, (_, i) => ({ id: "a" + String(i).padStart(3, "0"), title: "t", category: "", updated_at: "2026-10-01T00:00:00Z", hidden: false }));
  assert.equal(libraryBody([{ org: "acme", items: many }]).orgs[0].artifacts.length, 200);
});
