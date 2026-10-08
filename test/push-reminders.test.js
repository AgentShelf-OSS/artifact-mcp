// SPDX-License-Identifier: Apache-2.0
// ADR-0012: scheduled reminders and Web Push subscriptions in the Node reference.
import test from "node:test";
import assert from "node:assert/strict";
import Database from "better-sqlite3";
import { IncomingMessage, ServerResponse } from "node:http";
import { Duplex } from "node:stream";
import { createECDH, createHash } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { migrateDatabase } from "../lib/migrations.js";
import { createApp } from "../lib/app.js";
import { parseWebPushConfig } from "../lib/config.js";
import {
  createPushStore,
  hostAllowed,
  reminderFireAt,
  reminderText,
  unpackEndpointCiphertext,
  validPushEndpoint,
  validPushKeys
} from "../lib/push.js";

process.env.AUDIT_LEDGER_HMAC_KEY = Buffer.alloc(32, 7).toString("base64");
const importDir = mkdtempSync(join(tmpdir(), "artifact-push-reminders-"));
process.env.DATA_DIR = importDir;
const { handleMcp } = await import("../lib/mcp.js");
const { default: defaultDb } = await import("../lib/db.js");
const databases = new Set();
test.afterEach(() => { for (const db of databases) db.close(); databases.clear(); });
test.after(() => { defaultDb.close(); rmSync(importDir, { recursive: true, force: true }); });

const ENC_KEY = Buffer.alloc(32, 9).toString("base64");
const VAPID_KEY = Buffer.alloc(32, 1).toString("base64url");
const ENABLED_ENV = { WEB_PUSH_VAPID_PRIVATE_KEY: VAPID_KEY, WEB_PUSH_SUBJECT: "mailto:ops@example.test", WEBHOOK_ENC_KEY: ENC_KEY };
const ENABLED = parseWebPushConfig(ENABLED_ENV);
const DISABLED = parseWebPushConfig({});
const NOW = 1_800_000_000_000;

function clientKeys(seed = 2) {
  const ecdh = createECDH("prime256v1");
  ecdh.setPrivateKey(Buffer.alloc(32, seed));
  return { p256dh: ecdh.getPublicKey(null, "uncompressed").toString("base64url"), auth: Buffer.alloc(16, seed).toString("base64url") };
}
const endpoint = (suffix = "a") => `https://fcm.googleapis.com/fcm/send/${suffix}`;

function invoke(app, method, url, { body, rawBody, headers = {} } = {}) {
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
      resolve({ status: res.statusCode, body: value, headers: res.getHeaders(), raw: text }); socket.destroy(); return res;
    };
    req.on("error", reject);
    req.push(payload || null); if (payload) req.push(null);
    app.handle(req, res, (error) => reject(error || new Error("unhandled test route")));
  });
}

function fixture({ config = ENABLED, viewer = { email: "viewer@acme.test", org: "acme", isAdmin: false }, limit, clock = () => NOW } = {}) {
  const database = new Database(":memory:");
  databases.add(database);
  database.pragma("foreign_keys = ON");
  migrateDatabase(database);
  for (const [id, org] of [["owned", "acme"], ["foreign", "beta"], ["second", "acme"]]) {
    database.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES (?, 'publisher', ?, ?)").run(id, org, id);
  }
  let counter = 0;
  const push = createPushStore({ db: database, config, now: () => `2026-10-08 00:00:${String(counter).padStart(2, "0")}`, clock, newId: () => `sub-${++counter}` });
  const metas = { owned: { id: "owned", org: "acme" }, foreign: { id: "foreign", org: "beta" }, second: { id: "second", org: "acme" } };
  const app = createApp({
    push,
    limits: { statePerWindow: limit ?? 1000 },
    resolveViewer: async (req) => ({
      ...viewer,
      email: req.headers["test-viewer-email"] ?? viewer.email,
      org: req.headers["test-viewer-org"] ?? viewer.org,
      isAdmin: req.headers["test-viewer-admin"] === "1" || viewer.isAdmin
    }),
    artifacts: { getArtifactMeta: (id) => metas[id] || null },
    feedback: { listForArtifact: () => [] },
    pages: { notFound: () => "not found", notSignedIn: () => "not signed in", gallery: () => "", shell: () => "", settings: () => "" },
    logger: { error() {}, warn() {}, info() {} }
  });
  return { app, database, push };
}

test("Web Push config is off without a key, derives the public key, and needs WEBHOOK_ENC_KEY", () => {
  assert.equal(DISABLED.enabled, false);
  assert.equal(DISABLED.vapidPublicKey, null);
  assert.deepEqual(DISABLED.endpointHosts, ["fcm.googleapis.com", "updates.push.services.mozilla.com", "push.services.mozilla.com", "web.push.apple.com", "*.push.apple.com", "*.notify.windows.com"]);
  assert.equal(ENABLED.enabled, true);
  const ecdh = createECDH("prime256v1");
  ecdh.setPrivateKey(Buffer.alloc(32, 1));
  assert.equal(ENABLED.vapidPublicKey, ecdh.getPublicKey(null, "uncompressed").toString("base64url"));
  assert.equal(Buffer.from(ENABLED.vapidPublicKey, "base64url").length, 65);
  assert.equal(parseWebPushConfig({ ...ENABLED_ENV, WEBHOOK_ENC_KEY: "" }).enabled, false);
  assert.equal(parseWebPushConfig({ ...ENABLED_ENV, WEB_PUSH_SUBJECT: "https://example.test/contact" }).enabled, true);
  assert.deepEqual(parseWebPushConfig({ WEB_PUSH_ENDPOINT_HOSTS: " Push.Example.test, *.Example.org ,," }).endpointHosts, ["push.example.test", "*.example.org"]);
});

test("Web Push config rejects invalid keys, subjects, and host lists", () => {
  const n = Buffer.from("FFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551", "hex");
  for (const key of [Buffer.alloc(32, 0).toString("base64url"), n.toString("base64url"), Buffer.alloc(32, 0xff).toString("base64url"),
    Buffer.alloc(31, 1).toString("base64url"), Buffer.alloc(33, 1).toString("base64url"), Buffer.alloc(32, 1).toString("base64"), `${VAPID_KEY}=`, "not a key"]) {
    assert.throws(() => parseWebPushConfig({ ...ENABLED_ENV, WEB_PUSH_VAPID_PRIVATE_KEY: key }), /WEB_PUSH_VAPID_PRIVATE_KEY/, key);
  }
  for (const subject of ["", "ops@example.test", "mailto:", "http://example.test", "https://", "https://user:pw@example.test", "mailto:a b@example.test"]) {
    assert.throws(() => parseWebPushConfig({ ...ENABLED_ENV, WEB_PUSH_SUBJECT: subject }), /WEB_PUSH_SUBJECT/, subject);
  }
  for (const hosts of ["https://fcm.googleapis.com", "fcm.googleapis.com:443", "*", "*.", "a..b", "**.example.test", "ex ample.test"]) {
    assert.throws(() => parseWebPushConfig({ WEB_PUSH_ENDPOINT_HOSTS: hosts }), /WEB_PUSH_ENDPOINT_HOSTS/, hosts);
  }
  assert.throws(() => parseWebPushConfig({ ...ENABLED_ENV, WEBHOOK_ENC_KEY: "short" }), /WEBHOOK_ENC_KEY/);
});

test("endpoint allowlist enforces https, port 443, no userinfo, length, and *.suffix subdomains", () => {
  const hosts = ENABLED.endpointHosts;
  for (const ok of [endpoint(), "https://web.push.apple.com/abc", "https://api.push.apple.com/3/device/x", "https://wns2-db5p.notify.windows.com/w/?token=x", "https://fcm.googleapis.com:443/fcm/send/x", "https://FCM.googleapis.com/x"]) {
    assert.equal(validPushEndpoint(ok, hosts), true, ok);
  }
  for (const bad of ["http://fcm.googleapis.com/x", "https://fcm.googleapis.com:8443/x", "https://user@fcm.googleapis.com/x", "https://u:p@fcm.googleapis.com/x",
    "https://push.apple.com/x", "https://evil.test/x", "https://fcm.googleapis.com.evil.test/x", "https://evilfcm.googleapis.com/x", "https://127.0.0.1/x",
    `https://fcm.googleapis.com/${"a".repeat(1000)}`, "not a url", "", 42, null]) {
    assert.equal(validPushEndpoint(bad, hosts), false, String(bad));
  }
  assert.equal(validPushEndpoint(`https://fcm.googleapis.com/${"a".repeat(1024 - 27)}`, hosts), true);
  assert.equal(hostAllowed("a.b.push.apple.com", ["*.push.apple.com"]), true);
  assert.equal(hostAllowed("push.apple.com", ["*.push.apple.com"]), false);
});

test("subscription keys must be a valid P-256 point and a 16-byte auth secret", () => {
  const keys = clientKeys();
  assert.equal(validPushKeys(keys), true);
  assert.equal(validPushKeys({ ...keys, p256dh: `${keys.p256dh}=` }), true, "padding is tolerated");
  const offCurve = Buffer.from(keys.p256dh, "base64url"); offCurve[64] ^= 1;
  const compressed = createECDH("prime256v1"); compressed.setPrivateKey(Buffer.alloc(32, 3));
  for (const bad of [{ ...keys, p256dh: offCurve.toString("base64url") }, { ...keys, p256dh: compressed.getPublicKey(null, "compressed").toString("base64url") },
    { ...keys, p256dh: Buffer.alloc(65, 4).toString("base64url") }, { ...keys, auth: Buffer.alloc(15).toString("base64url") },
    { ...keys, auth: Buffer.alloc(16).toString("base64") + "+" }, { ...keys, p256dh: 5 }, { p256dh: keys.p256dh }, null, []]) {
    assert.equal(validPushKeys(bad), false, JSON.stringify(bad));
  }
});

test("reminder time and text validation follow the shared contract", () => {
  assert.equal(reminderFireAt({ delaySeconds: 60 }, NOW), NOW + 60_000);
  assert.equal(reminderFireAt({ delaySeconds: 59 }, NOW), null);
  assert.equal(reminderFireAt({ fireAt: NOW + 30 * 86_400_000 }, NOW), NOW + 30 * 86_400_000);
  assert.equal(reminderFireAt({ fireAt: NOW + 30 * 86_400_000 + 1 }, NOW), null);
  assert.equal(reminderFireAt({ fireAt: NOW + 60_000, delaySeconds: 120 }, NOW), null);
  assert.equal(reminderFireAt({}, NOW), null);
  assert.equal(reminderFireAt({ delaySeconds: 61.5 }, NOW), null);
  assert.equal(reminderFireAt({ fireAt: "1800000060000" }, NOW), null);
  assert.equal(reminderFireAt({ fireAt: NOW - 1 }, NOW), null);
  assert.deepEqual(reminderText("  Diaper  ", undefined), { title: "Diaper", body: "" });
  assert.deepEqual(reminderText("x".repeat(80), "line one\nline two"), { title: "x".repeat(80), body: "line one\nline two" });
  assert.deepEqual(reminderText("😀".repeat(80), "é".repeat(240)), { title: "😀".repeat(80), body: "é".repeat(240) });
  for (const [title, body] of [["   ", ""], ["x".repeat(81), ""], ["ok", "x".repeat(241)], ["tab\there", ""], ["ok", "bell\u0007"], ["ok", "c1\u0085"], ["ok", "cr\r\n"], [5, ""], ["ok", 5], [null, ""]]) {
    assert.equal(reminderText(title, body), null, JSON.stringify([title, body]));
  }
});

test("push routes are concealed, guarded, and disabled consistently", async () => {
  const { app } = fixture();
  const routes = [["get", "/foreign/push"], ["put", "/foreign/push/optin"], ["delete", "/foreign/push/optin"], ["get", "/foreign/reminders"],
    ["put", "/foreign/reminders/k", { title: "x", delay_seconds: 120 }], ["delete", "/foreign/reminders/k"]];
  for (const [method, path, body] of routes) {
    for (const target of [path, path.replace("foreign", "missing")]) {
      const result = await invoke(app, method, target, { body });
      assert.equal(result.status, 404, `${method} ${target}`);
      assert.deepEqual(result.body, { error: "Not found" });
    }
    const unsigned = await invoke(app, method, path.replace("foreign", "owned"), { body, headers: { "test-viewer-email": "" } });
    assert.equal(unsigned.status, 404);
  }
  for (const [method, path, body] of [...routes.map(([m, p, b]) => [m, p.replace("foreign", "owned"), b]), ["put", "/push/subscriptions", { endpoint: endpoint(), keys: clientKeys() }], ["delete", "/push/subscriptions", { endpoint: endpoint() }]]) {
    if (method === "get") continue;
    for (const headers of [{ "sec-fetch-site": "cross-site" }, { "x-artifact-mutation": "0" }, { "sec-fetch-site": "none", origin: "null" }]) {
      const result = await invoke(app, method, path, { body, headers });
      assert.equal(result.status, 403, `${method} ${path} ${JSON.stringify(headers)}`);
    }
  }
  assert.equal((await invoke(app, "get", "/push/config", { headers: { "test-viewer-email": "" } })).status, 404);

  const disabled = fixture({ config: DISABLED }).app;
  const config = await invoke(disabled, "get", "/push/config");
  assert.equal(config.status, 200);
  assert.deepEqual(config.body, { enabled: false, vapid_public_key: null });
  assert.equal(config.headers["cache-control"], "no-store");
  for (const [method, path, body] of [...routes.map(([m, p, b]) => [m, p.replace("foreign", "owned"), b]), ["put", "/push/subscriptions", { endpoint: endpoint(), keys: clientKeys() }], ["delete", "/push/subscriptions", { endpoint: endpoint() }]]) {
    const result = await invoke(disabled, method, path, { body });
    assert.equal(result.status, 404, `${method} ${path}`);
    assert.deepEqual(result.body, { error: "push_disabled" });
    assert.equal(result.headers["cache-control"], "no-store");
  }
});

test("push config, subscriptions, and opt-ins round trip with no-store responses", async () => {
  const { app, database, push } = fixture();
  let result = await invoke(app, "get", "/push/config");
  assert.deepEqual(result.body, { enabled: true, vapid_public_key: ENABLED.vapidPublicKey });
  result = await invoke(app, "get", "/owned/push");
  assert.deepEqual(result.body, { enabled: true, opted_in: false, devices: 0 });
  assert.equal(result.headers["cache-control"], "no-store");

  const keys = clientKeys();
  result = await invoke(app, "put", "/push/subscriptions", { body: { endpoint: endpoint(), keys, label: "iPhone", expirationTime: null } });
  assert.equal(result.status, 200);
  assert.deepEqual(result.body, { id: "sub-1" });
  const row = database.prepare("SELECT * FROM push_subscriptions").get();
  assert.equal(row.org, "acme");
  assert.equal(row.viewer_email, "viewer@acme.test");
  assert.equal(row.endpoint_sha256, createHash("sha256").update(endpoint()).digest("hex"));
  assert.equal(row.label, "iPhone");
  assert.ok(!row.endpoint_ciphertext.includes("fcm.googleapis.com"), "endpoint is encrypted at rest");
  assert.equal(unpackEndpointCiphertext(row.endpoint_ciphertext).nonce.length, 16);
  assert.equal(push.decryptEndpoint(row), endpoint());

  result = await invoke(app, "put", "/owned/push/optin", { body: {} });
  assert.deepEqual(result.body, { opted_in: true });
  result = await invoke(app, "put", "/owned/push/optin");
  assert.deepEqual(result.body, { opted_in: true }, "opt-in is idempotent");
  assert.deepEqual(database.prepare("SELECT artifact_id, viewer_email, org FROM artifact_push_optins").all(), [{ artifact_id: "owned", viewer_email: "viewer@acme.test", org: "acme" }]);
  result = await invoke(app, "get", "/owned/push");
  assert.deepEqual(result.body, { enabled: true, opted_in: true, devices: 1 });
  result = await invoke(app, "get", "/owned/push", { headers: { "test-viewer-email": "other@acme.test" } });
  assert.deepEqual(result.body, { enabled: true, opted_in: false, devices: 0 });
  result = await invoke(app, "delete", "/owned/push/optin");
  assert.deepEqual(result.body, { opted_in: false });
  assert.equal(database.prepare("SELECT COUNT(*) FROM artifact_push_optins").pluck().get(), 0);

  // Another viewer cannot delete this device; the route still answers 204.
  result = await invoke(app, "delete", "/push/subscriptions", { body: { endpoint: endpoint() }, headers: { "test-viewer-email": "other@acme.test" } });
  assert.equal(result.status, 204);
  assert.equal(database.prepare("SELECT COUNT(*) FROM push_subscriptions").pluck().get(), 1);
  result = await invoke(app, "delete", "/push/subscriptions", { body: { endpoint: endpoint() } });
  assert.equal(result.status, 204);
  assert.equal(database.prepare("SELECT COUNT(*) FROM push_subscriptions").pluck().get(), 0);
  result = await invoke(app, "delete", "/push/subscriptions", { body: { endpoint: endpoint("never") } });
  assert.equal(result.status, 204);
});

test("subscription validation errors and body shape", async () => {
  const { app } = fixture();
  const keys = clientKeys();
  const cases = [
    [{ endpoint: "http://fcm.googleapis.com/x", keys }, "bad_endpoint"],
    [{ endpoint: "https://fcm.googleapis.com:444/x", keys }, "bad_endpoint"],
    [{ endpoint: "https://u@fcm.googleapis.com/x", keys }, "bad_endpoint"],
    [{ endpoint: "https://evil.test/x", keys }, "bad_endpoint"],
    [{ keys }, "bad_endpoint"],
    [{ endpoint: endpoint(), keys: { ...keys, p256dh: Buffer.alloc(65, 4).toString("base64url") } }, "bad_keys"],
    [{ endpoint: endpoint(), keys: { ...keys, auth: "AAAA" } }, "bad_keys"],
    [{ endpoint: endpoint(), keys: { ...keys, extra: "x" } }, "bad_keys"],
    [{ endpoint: endpoint() }, "bad_keys"],
    [{ endpoint: endpoint(), keys, label: "x".repeat(61) }, "bad_body"],
    [{ endpoint: endpoint(), keys, label: "bad\u0000" }, "bad_body"],
    [{ endpoint: endpoint(), keys, surprise: true }, "bad_body"],
    [[], "bad_body"]
  ];
  for (const [body, error] of cases) {
    const result = await invoke(app, "put", "/push/subscriptions", { body });
    assert.equal(result.status, 400, JSON.stringify(body));
    assert.deepEqual(result.body, { error }, JSON.stringify(body));
  }
  const malformed = await invoke(app, "put", "/push/subscriptions", { rawBody: "{" });
  assert.deepEqual([malformed.status, malformed.body], [400, { error: "bad_body" }]);
  const deleteMalformed = await invoke(app, "delete", "/push/subscriptions", { body: { endpoint: 5 } });
  assert.deepEqual([deleteMalformed.status, deleteMalformed.body], [400, { error: "bad_body" }]);
});

test("subscriptions dedupe by endpoint, move between viewers, and cap at ten per viewer", async () => {
  const { app, database } = fixture();
  for (let index = 0; index < 10; index += 1) {
    const result = await invoke(app, "put", "/push/subscriptions", { body: { endpoint: endpoint(`d${index}`), keys: clientKeys(index + 2) } });
    assert.equal(result.status, 200);
  }
  // Re-registering the same endpoint keeps the row and its id.
  let result = await invoke(app, "put", "/push/subscriptions", { body: { endpoint: endpoint("d0"), keys: clientKeys(40), label: "Pixel" } });
  assert.deepEqual(result.body, { id: "sub-1" });
  assert.equal(database.prepare("SELECT COUNT(*) FROM push_subscriptions").pluck().get(), 10);
  assert.equal(database.prepare("SELECT p256dh FROM push_subscriptions WHERE id = 'sub-1'").pluck().get(), clientKeys(40).p256dh);
  result = await invoke(app, "put", "/push/subscriptions", { body: { endpoint: endpoint("d10"), keys: clientKeys(50) } });
  assert.equal(result.status, 200);
  const ids = database.prepare("SELECT id FROM push_subscriptions WHERE viewer_email = 'viewer@acme.test' ORDER BY created_at, rowid").pluck().all();
  assert.equal(ids.length, 10);
  assert.ok(!ids.includes("sub-1"), "the oldest row is deleted");
  // The same endpoint registered by another viewer moves to that viewer and org.
  result = await invoke(app, "put", "/push/subscriptions", { body: { endpoint: endpoint("d5"), keys: clientKeys(60) }, headers: { "test-viewer-email": "Second@Beta.test", "test-viewer-org": "beta" } });
  assert.equal(result.status, 200);
  assert.deepEqual(database.prepare("SELECT org, viewer_email FROM push_subscriptions WHERE id = ?").get(result.body.id), { org: "beta", viewer_email: "second@beta.test" });
  assert.equal(database.prepare("SELECT COUNT(*) FROM push_subscriptions WHERE viewer_email = 'viewer@acme.test'").pluck().get(), 9);
});

test("reminder routes upsert with revisions, enforce limits, and isolate viewer scope", async () => {
  const { app, database } = fixture();
  let result = await invoke(app, "put", "/owned/reminders/diaper", { body: { delay_seconds: 7200, title: "  Diaper change due ", body: "Check the dressing." } });
  assert.equal(result.status, 200);
  assert.deepEqual(result.body, { key: "diaper", scope: "org", fire_at: NOW + 7_200_000, revision: 1 });
  assert.equal(result.headers["cache-control"], "no-store");
  result = await invoke(app, "put", "/owned/reminders/diaper", { body: { fire_at: NOW + 60_000, title: "Again" } });
  assert.deepEqual(result.body, { key: "diaper", scope: "org", fire_at: NOW + 60_000, revision: 2 });
  const row = database.prepare("SELECT * FROM artifact_reminders").get();
  assert.equal(row.owner, "");
  assert.equal(row.org, "acme");
  assert.equal(row.body, "");
  assert.equal(row.created_by, "viewer");
  assert.equal(row.state, "armed");
  // A fired row is re-armed by the next write and fired_at is cleared.
  database.prepare("UPDATE artifact_reminders SET state = 'fired', fired_at = 'x'").run();
  result = await invoke(app, "get", "/owned/reminders");
  assert.deepEqual(result.body, { reminders: [] }, "fired rows are not listed");
  result = await invoke(app, "put", "/owned/reminders/diaper", { body: { delay_seconds: 120, title: "Re-armed" } });
  assert.equal(result.body.revision, 3);
  assert.deepEqual(database.prepare("SELECT state, fired_at FROM artifact_reminders").get(), { state: "armed", fired_at: null });

  result = await invoke(app, "put", "/owned/reminders/mine?scope=viewer", { body: { delay_seconds: 300, title: "Private" }, headers: { "test-viewer-email": "Viewer@Acme.test" } });
  assert.deepEqual(result.body, { key: "mine", scope: "viewer", fire_at: NOW + 300_000, revision: 1 });
  assert.equal(database.prepare("SELECT owner FROM artifact_reminders WHERE key = 'mine'").pluck().get(), "viewer@acme.test");
  result = await invoke(app, "get", "/owned/reminders?scope=viewer");
  assert.deepEqual(result.body.reminders, [{ key: "mine", scope: "viewer", fire_at: NOW + 300_000, title: "Private", body: "", revision: 1 }]);
  result = await invoke(app, "get", "/owned/reminders?scope=viewer", { headers: { "test-viewer-email": "other@acme.test" } });
  assert.deepEqual(result.body, { reminders: [] });
  result = await invoke(app, "delete", "/owned/reminders/mine?scope=viewer", { headers: { "test-viewer-email": "other@acme.test" } });
  assert.equal(result.status, 204);
  assert.equal(database.prepare("SELECT COUNT(*) FROM artifact_reminders WHERE key = 'mine'").pluck().get(), 1, "another viewer cannot clear it");
  result = await invoke(app, "get", "/owned/reminders");
  assert.deepEqual(result.body.reminders.map((entry) => entry.key), ["diaper"], "org listing excludes viewer rows");

  // 16 armed rows per artifact across scopes.
  for (let index = 0; index < 14; index += 1) {
    result = await invoke(app, "put", `/owned/reminders/k${index}`, { body: { delay_seconds: 600, title: `R${index}` } });
    assert.equal(result.status, 200);
  }
  result = await invoke(app, "put", "/owned/reminders/overflow", { body: { delay_seconds: 600, title: "Too many" } });
  assert.deepEqual([result.status, result.body], [409, { error: "reminder_limit" }]);
  result = await invoke(app, "put", "/owned/reminders/k3", { body: { delay_seconds: 900, title: "Existing key" } });
  assert.equal(result.status, 200, "an existing armed key does not count as new");
  result = await invoke(app, "put", "/second/reminders/other", { body: { delay_seconds: 600, title: "Other artifact" } });
  assert.equal(result.status, 200, "the limit is per artifact");
  result = await invoke(app, "delete", "/owned/reminders/k3");
  assert.equal(result.status, 204);
  result = await invoke(app, "delete", "/owned/reminders/k3");
  assert.equal(result.status, 204, "absent rows still answer 204");
  result = await invoke(app, "put", "/owned/reminders/overflow", { body: { delay_seconds: 600, title: "Fits now" } });
  assert.equal(result.status, 200);
});

test("reminder route validation errors", async () => {
  const { app } = fixture();
  const cases = [
    ["/owned/reminders/k", { title: "x" }, "bad_time"],
    ["/owned/reminders/k", { title: "x", delay_seconds: 59 }, "bad_time"],
    ["/owned/reminders/k", { title: "x", delay_seconds: 30 * 86_400 + 1 }, "bad_time"],
    ["/owned/reminders/k", { title: "x", delay_seconds: 60, fire_at: NOW + 120_000 }, "bad_time"],
    ["/owned/reminders/k", { title: "x", fire_at: NOW + 59_999 }, "bad_time"],
    ["/owned/reminders/k", { title: "x", delay_seconds: "120" }, "bad_time"],
    ["/owned/reminders/k", { title: "x", delay_seconds: null }, "bad_time"],
    ["/owned/reminders/k", { delay_seconds: 120 }, "bad_text"],
    ["/owned/reminders/k", { title: " ", delay_seconds: 120 }, "bad_text"],
    ["/owned/reminders/k", { title: "x".repeat(81), delay_seconds: 120 }, "bad_text"],
    ["/owned/reminders/k", { title: "x", body: "y".repeat(241), delay_seconds: 120 }, "bad_text"],
    ["/owned/reminders/k", { title: "a\u001bb", delay_seconds: 120 }, "bad_text"],
    ["/owned/reminders/k", { title: "x", body: null, delay_seconds: 120 }, "bad_text"],
    ["/owned/reminders/k", { title: "x", delay_seconds: 120, extra: 1 }, "bad_body"],
    ["/owned/reminders/k?scope=team", { title: "x", delay_seconds: 120 }, "bad_scope"],
    ["/owned/reminders/k?scope=org&scope=viewer", { title: "x", delay_seconds: 120 }, "bad_scope"],
    [`/owned/reminders/${"k".repeat(65)}`, { title: "x", delay_seconds: 120 }, "bad_key"],
    ["/owned/reminders/bad%20key", { title: "x", delay_seconds: 120 }, "bad_key"]
  ];
  for (const [path, body, error] of cases) {
    const result = await invoke(app, "put", path, { body });
    assert.equal(result.status, 400, `${path} ${JSON.stringify(body)}`);
    assert.deepEqual(result.body, { error }, `${path} ${JSON.stringify(body)}`);
  }
  const list = await invoke(app, "get", "/owned/reminders?scope=nope");
  assert.deepEqual([list.status, list.body], [400, { error: "bad_scope" }]);
  const malformed = await invoke(app, "put", "/owned/reminders/k", { rawBody: "[" });
  assert.deepEqual([malformed.status, malformed.body], [400, { error: "bad_body" }]);
});

test("push mutations share the viewer-state rate-limit budget", async () => {
  const { app } = fixture({ limit: 2 });
  assert.equal((await invoke(app, "put", "/owned/push/optin")).status, 200);
  assert.equal((await invoke(app, "put", "/owned/reminders/a", { body: { title: "x", delay_seconds: 120 } })).status, 200);
  const limited = await invoke(app, "delete", "/owned/reminders/a");
  assert.deepEqual([limited.status, limited.body], [429, { error: "rate_limited" }]);
  assert.equal((await invoke(app, "get", "/owned/reminders")).status, 200, "reads do not spend the budget");
});

test("admins may arm reminders on any organization's artifact with that artifact's org", async () => {
  const { app, database } = fixture({ viewer: { email: "admin@example.test", org: "admin", isAdmin: true } });
  const result = await invoke(app, "put", "/foreign/reminders/k", { body: { title: "Admin", delay_seconds: 120 } });
  assert.equal(result.status, 200);
  assert.equal(database.prepare("SELECT org FROM artifact_reminders").pluck().get(), "beta");
});

test("the service worker and manifest are served with contract headers before artifact routes", async () => {
  for (const config of [ENABLED, DISABLED]) {
    const { app } = fixture({ config });
    const sw = await invoke(app, "get", "/sw.js");
    assert.equal(sw.status, 200);
    assert.equal(sw.headers["content-type"], "text/javascript; charset=utf-8");
    assert.equal(sw.headers["cache-control"], "no-cache");
    assert.equal(sw.headers["service-worker-allowed"], "/");
    assert.equal(sw.raw, readFileSync(new URL("../assets/push-sw.js", import.meta.url), "utf8"));
    const manifest = await invoke(app, "get", "/manifest.webmanifest");
    assert.equal(manifest.status, 200);
    assert.equal(manifest.headers["content-type"], "application/manifest+json");
    assert.equal(manifest.headers["cache-control"], "no-cache");
    assert.equal(manifest.raw, readFileSync(new URL("../assets/manifest.webmanifest", import.meta.url), "utf8"));
    for (const name of ["app-192.png", "app-512.png", "maskable-512.png", "apple-touch-icon.png", "badge-72.png"]) {
      const icon = await invoke(app, "get", `/icons/${name}`, { headers: { cookie: "", "test-viewer-email": "" } });
      assert.equal(icon.status, 200, name);
      assert.equal(icon.headers["content-type"], "image/png");
      assert.equal(icon.headers["cache-control"], "public, max-age=3600");
      assert.equal(icon.headers["x-content-type-options"], "nosniff");
    }
    for (const path of ["/icons/missing.png", "/icons/app-192.png/x", "/icons/..%2fpush-sw.js"]) {
      assert.equal((await invoke(app, "get", path)).status, 404, path);
    }
    // No viewer is required for these static files.
    for (const path of ["/sw.js", "/manifest.webmanifest"]) {
      assert.equal((await invoke(app, "get", path, { headers: { cookie: "", "test-viewer-email": "" } })).status, 200, path);
    }
  }
});

test("migration creates the ADR-0012 tables and cascades artifact deletion", () => {
  const { database, push } = fixture();
  for (const table of ["push_subscriptions", "artifact_push_optins", "artifact_reminders", "push_deliveries"]) {
    assert.equal(database.prepare("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?").pluck().get(table), 1, table);
  }
  assert.equal(database.prepare("SELECT name FROM schema_migrations WHERE version = 39").pluck().get(), "web-push-reminders");
  push.optIn("owned", "acme", "viewer@acme.test");
  push.setReminder({ artifactId: "owned", org: "acme", key: "k", delaySeconds: 120, title: "x", createdBy: "viewer" });
  database.prepare("DELETE FROM artifacts WHERE id = 'owned'").run();
  assert.equal(database.prepare("SELECT COUNT(*) FROM artifact_reminders").pluck().get(), 0);
  assert.equal(database.prepare("SELECT COUNT(*) FROM artifact_push_optins").pluck().get(), 0);
  assert.throws(() => database.prepare("INSERT INTO artifact_reminders (artifact_id, scope, key, org, fire_at, title, body, created_by, updated_at) VALUES ('second', 'team', 'k', 'acme', 1, 't', '', 'viewer', 'now')").run(), /CHECK/);
});

test("set_artifact_reminder arms, replaces, clears, and refuses when disabled", async () => {
  const author = { clientId: "reminder-publisher", org: "acme", label: "Agent" };
  const published = await handleMcp({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "publish_artifact", arguments: { html: "<p>log</p>", title: "Log" } } }, author);
  const id = published.result.structuredContent.id;
  const reminders = createPushStore({ db: defaultDb, config: ENABLED });
  const call = (args, identity = author, store = reminders) => handleMcp({ jsonrpc: "2.0", id: 2, method: "tools/call", params: { name: "set_artifact_reminder", arguments: args } }, identity, { reminders: store });

  let response = await call({ id, key: "diaper", delay_seconds: 7200, title: "Diaper change due", body: "Two hours since the last change." });
  assert.equal(response.result.isError, undefined, JSON.stringify(response));
  const armed = response.result.structuredContent;
  assert.deepEqual(Object.keys(armed).sort(), ["fire_at", "id", "key", "revision", "scope"]);
  assert.equal(armed.scope, "org");
  assert.equal(armed.revision, 1);
  assert.ok(Math.abs(armed.fire_at - (Date.now() + 7_200_000)) < 10_000);
  assert.deepEqual(JSON.parse(response.result.content[0].text), armed);
  const row = defaultDb.prepare("SELECT scope, owner, org, created_by FROM artifact_reminders WHERE artifact_id = ? AND key = 'diaper'").get(id);
  assert.deepEqual(row, { scope: "org", owner: "", org: "acme", created_by: "publisher:reminder-publisher" });

  response = await call({ id, key: "diaper", fire_at: Date.now() + 3_600_000, title: "Replaced", clear: false });
  assert.equal(response.result.structuredContent.revision, 2);

  for (const [args, message] of [
    [{ id, key: "diaper", title: "x" }, /Invalid reminder time/],
    [{ id, key: "diaper", delay_seconds: 10, title: "x" }, /Invalid reminder time/],
    [{ id, key: "diaper", delay_seconds: 120, title: "" }, /Invalid reminder text/],
    [{ id, key: "bad key", delay_seconds: 120, title: "x" }, /Invalid reminder key/]
  ]) {
    response = await call(args);
    assert.equal(response.result.isError, true, JSON.stringify(args));
    assert.match(response.result.content[0].text, message);
  }

  response = await call({ id, key: "diaper", clear: true, title: "", delay_seconds: 1 });
  assert.deepEqual(response.result.structuredContent, { id, key: "diaper", cleared: true });
  assert.equal(defaultDb.prepare("SELECT COUNT(*) FROM artifact_reminders WHERE artifact_id = ?").pluck().get(id), 0);
  response = await call({ id, key: "diaper", clear: true });
  assert.deepEqual(response.result.structuredContent, { id, key: "diaper", cleared: true }, "clearing an absent key succeeds");

  response = await call({ id, key: "diaper", delay_seconds: 120, title: "x" }, { clientId: "intruder", org: "beta", label: "Other" });
  assert.equal(response.result.isError, true);
  assert.doesNotMatch(response.result.content[0].text, /Web Push/);

  for (const store of [createPushStore({ db: defaultDb, config: DISABLED }), null]) {
    response = await call({ id, key: "diaper", delay_seconds: 120, title: "x" }, author, store);
    assert.equal(response.result.isError, true);
    assert.equal(response.result.content[0].text, "Web Push reminders are not configured on this server.");
  }

  response = await call({ id, key: "diaper", delay_seconds: 120, title: "x", scope: "viewer" });
  assert.equal(response.error.code, -32602, "scope is not an MCP argument");
});

test("review fixes: null time fields, host entry rules, and IP-literal endpoints", async () => {
  assert.equal(reminderFireAt({ fireAt: NOW + 120_000, delaySeconds: null }, NOW), null);
  assert.equal(reminderFireAt({ fireAt: null, delaySeconds: 120 }, NOW), null);
  assert.equal(reminderFireAt({ fireAt: null }, NOW), null);
  const { app } = fixture();
  for (const body of [{ title: "x", fire_at: NOW + 120_000, delay_seconds: null }, { title: "x", fire_at: null, delay_seconds: 120 }]) {
    const result = await invoke(app, "put", "/owned/reminders/k", { body });
    assert.deepEqual([result.status, result.body], [400, { error: "bad_time" }], JSON.stringify(body));
  }
  for (const hosts of ["*.com", "com", "localhost", "127.0.0.1", "10.0.0.1", "*.0.1", "push.123", "::1", "[::1]"]) {
    assert.throws(() => parseWebPushConfig({ WEB_PUSH_ENDPOINT_HOSTS: hosts }), /WEB_PUSH_ENDPOINT_HOSTS/, hosts);
  }
  assert.deepEqual(parseWebPushConfig({ WEB_PUSH_ENDPOINT_HOSTS: "*.example.com,push1.example.com" }).endpointHosts, ["*.example.com", "push1.example.com"]);
  for (const url of ["https://127.0.0.1/x", "https://[::1]/x", "https://2130706433/x", "https://1.2.3/x"]) {
    assert.equal(validPushEndpoint(url, ["127.0.0.1", "*.0.0.1", "[::1]", "2130706433"]), false, url);
  }
  assert.equal(hostAllowed("127.0.0.1", ["127.0.0.1"]), false);
});

test("review fixes: subscription org refresh, admin devices, and pending deliveries on handover", async () => {
  const { app, database } = fixture();
  let result = await invoke(app, "put", "/push/subscriptions", { body: { endpoint: endpoint("moved"), keys: clientKeys() } });
  const id = result.body.id;
  // The viewer's org changes; any authenticated push request moves their subscriptions.
  database.prepare("UPDATE push_subscriptions SET org = 'old-org'").run();
  result = await invoke(app, "get", "/owned/push");
  assert.equal(result.body.devices, 1);
  assert.equal(database.prepare("SELECT org FROM push_subscriptions WHERE id = ?").pluck().get(id), "acme");
  database.prepare("UPDATE push_subscriptions SET org = 'old-org'").run();
  await invoke(app, "get", "/push/config");
  assert.equal(database.prepare("SELECT org FROM push_subscriptions WHERE id = ?").pluck().get(id), "acme");

  // Administrators' subscriptions (org 'admin') count for every artifact org.
  const admin = { "test-viewer-email": "admin@example.test", "test-viewer-org": "admin", "test-viewer-admin": "1" };
  result = await invoke(app, "put", "/push/subscriptions", { body: { endpoint: endpoint("admin"), keys: clientKeys(5) }, headers: admin });
  const adminId = result.body.id;
  assert.equal(database.prepare("SELECT org FROM push_subscriptions WHERE id = ?").pluck().get(adminId), "admin");
  assert.equal((await invoke(app, "get", "/foreign/push", { headers: admin })).body.devices, 1);
  assert.equal((await invoke(app, "get", "/owned/push", { headers: admin })).body.devices, 1);

  // Handing a device to another viewer drops its pending deliveries, but keeps finished ones.
  const insert = database.prepare("INSERT INTO push_deliveries (subscription_id, artifact_id, reminder_key, payload, state, next_attempt_at, expires_at, created_at, updated_at) VALUES (?, 'owned', 'k', '{}', ?, 0, 0, 'now', 'now')");
  insert.run(id, "pending"); insert.run(id, "accepted");
  await invoke(app, "put", "/push/subscriptions", { body: { endpoint: endpoint("moved"), keys: clientKeys() } });
  assert.equal(database.prepare("SELECT COUNT(*) FROM push_deliveries WHERE state = 'pending'").pluck().get(), 1, "same owner keeps pending rows");
  await invoke(app, "put", "/push/subscriptions", { body: { endpoint: endpoint("moved"), keys: clientKeys() }, headers: { "test-viewer-email": "other@acme.test" } });
  assert.deepEqual(database.prepare("SELECT state FROM push_deliveries ORDER BY id").pluck().all(), ["accepted"]);
});
